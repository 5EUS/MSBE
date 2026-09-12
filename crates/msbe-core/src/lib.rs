//! Plan engine, solver and the resolve/apply pipeline.
//!
//! Knows nothing about any game. If a game name, store name, loader or file format
//! appears in this crate, that is a design bug: the test is that this crate compiles
//! and passes its suite with zero plans installed.
//!
//! See `docs/00-overview.md` and `docs/02-plan-system.md`.

pub mod bisect;
pub mod config;
pub mod instance;
pub mod solver;
use std::collections::BTreeMap;

use msbe_fsops::{Applier, Digest, Observer, Operation, RelPath, Result as FsResult, TxnReport};
use msbe_plan_schema::{
    EditJsonStep, ExtractStep, GAME_VERSION, Hygiene, InjectStep, Loader, Plan, Step,
    ValidationError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// One acquired file available to the resolver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedFile {
    /// The file's path inside its resolved source tree.
    pub source: RelPath,
    /// The content-addressed blob that holds the file's bytes.
    pub blob: Digest,
    /// Whether the game is expected to modify this file at runtime.
    pub mutable: bool,
}

/// The inert filesystem changes produced by resolving a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationSet {
    /// Operations in the order the applier must execute them.
    pub operations: Vec<Operation>,
    /// Source files deliberately withheld from deployment, with the responsible rule.
    pub excluded: Vec<ExcludedFile>,
}

/// A source file omitted from deployment without removing it from the content store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedFile {
    /// The source-tree path of the excluded file.
    pub source: RelPath,
    /// The rule that excluded the file.
    pub reason: ExclusionReason,
}

/// Why a source file is absent from the resolved deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExclusionReason {
    /// A conservative built-in OS, VCS, or debug-artifact rule matched.
    Hygiene,
    /// No allow pattern admitted the file.
    NotAllowed,
    /// A plan deny pattern matched the file.
    Denied {
        /// The matching glob pattern.
        pattern: String,
    },
    /// A plan quarantine pattern matched the file.
    Quarantined {
        /// The matching glob pattern.
        pattern: String,
    },
}

impl OperationSet {
    /// Applies these operations as one crash-recoverable filesystem transaction.
    ///
    /// This is intentionally a direct handoff: callers use [`resolve`] for dry runs and
    /// this method for execution, so the two paths cannot diverge.
    ///
    /// # Errors
    ///
    /// Returns any error from the transactional filesystem applier.
    pub fn apply(&self, applier: &mut Applier, observer: &mut dyn Observer) -> FsResult<TxnReport> {
        applier.apply(&self.operations, observer)
    }
}

/// Resolves already-acquired files into filesystem operations for one loader.
///
/// Resolution is pure: it validates only the manifest and paths, never reads or writes an
/// instance. The caller can therefore use its result for a dry run or pass it to the
/// transactional applier unchanged. Steps limited to other loaders do not apply.
///
/// # Errors
///
/// Returns [`ResolveError`] if the plan is invalid, the requested loader is unavailable, or
/// a step names a target the loader does not declare.
pub fn resolve(
    plan: &Plan,
    loader_id: &str,
    files: &[ResolvedFile],
) -> Result<OperationSet, ResolveError> {
    plan.validate()?;
    let loader = plan
        .loaders
        .iter()
        .find(|loader| loader.id == loader_id)
        .ok_or_else(|| ResolveError::UnknownLoader(loader_id.to_owned()))?;

    let (files, excluded) = filter_files(plan, loader_id, files);
    let mutable_globs = mutable_globs(plan, loader);
    let mut operations = Vec::new();
    for step in &plan.steps {
        let Step::Place(place) = step else {
            continue;
        };
        if !step.applies_to(loader_id) {
            continue;
        }
        let target_name = place
            .into
            .strip_prefix("@loader.targets.")
            .ok_or_else(|| ResolveError::InvalidTargetReference(place.into.clone()))?;
        let target = loader
            .targets
            .iter()
            .find(|target| target.name == target_name)
            .ok_or_else(|| ResolveError::UnknownTarget {
                loader: loader.id.clone(),
                target: target_name.to_owned(),
            })?;

        for file in &files {
            if !place.include.is_empty()
                && !place
                    .include
                    .iter()
                    .any(|pattern| matches_glob(pattern, file.source.as_str()))
            {
                continue;
            }
            if place
                .exclude
                .iter()
                .any(|pattern| matches_glob(pattern, file.source.as_str()))
            {
                continue;
            }
            let source = place.strip_prefix.as_deref().map_or_else(
                || Ok(file.source.as_str()),
                |prefix| {
                    file.source
                        .as_str()
                        .strip_prefix(&format!("{prefix}/"))
                        .ok_or_else(|| ResolveError::OutsidePlacementPrefix {
                            path: file.source.clone(),
                            prefix: prefix.to_owned(),
                        })
                },
            )?;
            let source = if place.flatten {
                source.rsplit('/').next().unwrap_or(source)
            } else {
                source
            };
            let path = RelPath::new(&format!("{}/{}", target.path, source))?;
            let mutable = file.mutable
                || mutable_globs
                    .iter()
                    .any(|glob| matches_glob(glob, path.as_str()));
            operations.push(Operation::Materialize {
                path,
                blob: file.blob,
                mutable,
            });
        }
    }
    Ok(OperationSet {
        operations,
        excluded,
    })
}

/// A container a plan builds from a file in the game and the profile's mods.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Injection {
    /// The zip container the result starts from, as it was before MSBE changed it.
    pub base: RelPath,
    /// Where the result is placed.
    pub into: RelPath,
    /// Globs over entries left out of the result.
    pub remove: Vec<String>,
    /// The files written into the container, in the order they apply: a later file with the
    /// same source path replaces an earlier one.
    pub entries: Vec<ResolvedFile>,
}

impl Injection {
    /// Whether the container entry at `path` is left out of the result.
    pub fn removes(&self, path: &str) -> bool {
        self.remove
            .iter()
            .any(|pattern| matches_glob(pattern, path))
    }
}

/// A JSON document a plan derives from a file in the game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonEdit {
    /// The JSON document the result starts from, as it was before MSBE changed it.
    pub base: RelPath,
    /// Where the result is placed.
    pub into: RelPath,
    /// JSON pointers and the strings they are set to, with placeholders filled in.
    pub set: BTreeMap<String, String>,
    /// JSON pointers deleted before any are set.
    pub remove: Vec<String>,
}

/// Files a plan builds from the game's own files rather than placing from a mod.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Derivations {
    /// Containers built by injection, in plan order.
    pub injections: Vec<Injection>,
    /// JSON documents derived by editing, in plan order.
    pub edits: Vec<JsonEdit>,
}

/// Works out the files a plan derives for one loader from files already in the game.
///
/// `mods` holds every mod's files in profile order, which is the order injected entries apply
/// in. Like [`resolve`] this is pure: the caller reads each base file and builds the result.
///
/// # Errors
///
/// Returns [`ResolveError`] if the plan is invalid, the loader is unknown, a template needs a
/// game version the instance does not have, or a filled-in path is unsafe.
pub fn derive(
    plan: &Plan,
    loader_id: &str,
    game_version: Option<&str>,
    mods: &[Vec<ResolvedFile>],
) -> Result<Derivations, ResolveError> {
    plan.validate()?;
    if !plan.loaders.iter().any(|loader| loader.id == loader_id) {
        return Err(ResolveError::UnknownLoader(loader_id.to_owned()));
    }
    let mut derivations = Derivations::default();
    for step in plan.steps.iter().filter(|step| step.applies_to(loader_id)) {
        match step {
            Step::Inject(inject) => {
                derivations.injections.push(injection(
                    plan,
                    loader_id,
                    inject,
                    game_version,
                    mods,
                )?);
            }
            Step::EditJson(edit) => derivations.edits.push(json_edit(edit, game_version)?),
            Step::Extract(_) | Step::Place(_) | Step::MergeConfig(_) => {}
        }
    }
    Ok(derivations)
}

fn injection(
    plan: &Plan,
    loader_id: &str,
    step: &InjectStep,
    game_version: Option<&str>,
    mods: &[Vec<ResolvedFile>],
) -> Result<Injection, ResolveError> {
    let entries = mods
        .iter()
        .flat_map(|files| filter_files(plan, loader_id, files).0)
        .filter(|file| {
            step.include.is_empty()
                || step
                    .include
                    .iter()
                    .any(|pattern| matches_glob(pattern, file.source.as_str()))
        })
        .cloned()
        .collect();
    Ok(Injection {
        base: template_path(&step.base, game_version)?,
        into: template_path(&step.into, game_version)?,
        remove: step.remove.clone(),
        entries,
    })
}

fn json_edit(step: &EditJsonStep, game_version: Option<&str>) -> Result<JsonEdit, ResolveError> {
    Ok(JsonEdit {
        base: template_path(&step.base, game_version)?,
        into: template_path(&step.into, game_version)?,
        set: step
            .set
            .iter()
            .map(|(pointer, value)| Ok((pointer.clone(), fill(value, game_version)?)))
            .collect::<Result<_, ResolveError>>()?,
        remove: step.remove.clone(),
    })
}

/// `template` with `{game_version}` filled in.
fn fill(template: &str, game_version: Option<&str>) -> Result<String, ResolveError> {
    if !template.contains(GAME_VERSION) {
        return Ok(template.to_owned());
    }
    let version =
        game_version.ok_or_else(|| ResolveError::GameVersionRequired(template.to_owned()))?;
    Ok(template.replace(GAME_VERSION, version))
}

fn template_path(template: &str, game_version: Option<&str>) -> Result<RelPath, ResolveError> {
    Ok(RelPath::new(&fill(template, game_version)?)?)
}

/// Applies `edit` to the JSON document `bytes`, returning pretty, newline-terminated JSON. The
/// same inputs always give the same bytes.
///
/// Removing a pointer that is absent changes nothing. Setting one requires its parent to be an
/// object.
///
/// # Errors
///
/// Returns [`ResolveError::InvalidJson`] if `bytes` is not JSON, or
/// [`ResolveError::JsonPointer`] if a pointer to set has no object to hold it.
pub fn edit_json(bytes: &[u8], edit: &JsonEdit) -> Result<Vec<u8>, ResolveError> {
    let invalid = |reason: String| ResolveError::InvalidJson {
        path: edit.base.clone(),
        reason,
    };
    let mut document: Value =
        serde_json::from_slice(bytes).map_err(|error| invalid(error.to_string()))?;
    for pointer in &edit.remove {
        let (parent, key) = split_pointer(pointer);
        if let Some(Value::Object(object)) = document.pointer_mut(parent) {
            object.remove(&key);
        }
    }
    for (pointer, value) in &edit.set {
        let (parent, key) = split_pointer(pointer);
        let Some(Value::Object(object)) = document.pointer_mut(parent) else {
            return Err(ResolveError::JsonPointer {
                path: edit.base.clone(),
                pointer: pointer.clone(),
            });
        };
        object.insert(key, Value::String(value.clone()));
    }
    let mut edited =
        serde_json::to_vec_pretty(&document).map_err(|error| invalid(error.to_string()))?;
    edited.push(b'\n');
    Ok(edited)
}

/// A JSON pointer's parent pointer and its final reference token, unescaped.
fn split_pointer(pointer: &str) -> (&str, String) {
    let (parent, token) = pointer.rsplit_once('/').unwrap_or(("", pointer));
    (parent, token.replace("~1", "/").replace("~0", "~"))
}

/// The plan's mutable globs with `@loader.targets.<name>` expanded for `loader`. A glob naming
/// a target this loader does not declare cannot match anything it places, so it is dropped.
fn mutable_globs(plan: &Plan, loader: &Loader) -> Vec<String> {
    plan.deploy
        .mutable
        .iter()
        .filter_map(|pattern| {
            let Some(reference) = pattern.strip_prefix("@loader.targets.") else {
                return Some(pattern.clone());
            };
            let (name, rest) = reference
                .split_once('/')
                .map_or((reference, None), |(name, rest)| (name, Some(rest)));
            let target = loader.targets.iter().find(|target| target.name == name)?;
            Some(rest.map_or_else(
                || target.path.clone(),
                |rest| format!("{}/{rest}", target.path),
            ))
        })
        .collect()
}

/// The files the loader's extract steps admit, and the rest with the rule that excluded each.
fn filter_files<'a>(
    plan: &Plan,
    loader_id: &str,
    files: &'a [ResolvedFile],
) -> (Vec<&'a ResolvedFile>, Vec<ExcludedFile>) {
    let extract_steps: Vec<&ExtractStep> = plan
        .steps
        .iter()
        .filter(|step| step.applies_to(loader_id))
        .filter_map(|step| match step {
            Step::Extract(extract) => Some(extract),
            Step::Place(_) | Step::MergeConfig(_) | Step::Inject(_) | Step::EditJson(_) => None,
        })
        .collect();
    if extract_steps.is_empty() {
        return (files.iter().collect(), Vec::new());
    }

    let mut included = Vec::new();
    let mut excluded = Vec::new();
    for file in files {
        let reason = extract_steps
            .iter()
            .find_map(|extract| exclusion_reason(extract, &file.source));
        if let Some(reason) = reason {
            excluded.push(ExcludedFile {
                source: file.source.clone(),
                reason,
            });
        } else {
            included.push(file);
        }
    }
    (included, excluded)
}

/// The rule that excludes `path`, most specific first: hygiene, then an explicit deny or
/// quarantine pattern, then a miss against the allow list. An explicit rule is the more useful
/// explanation in an exclusion report, so it wins over a generic "not allowed".
fn exclusion_reason(extract: &ExtractStep, path: &RelPath) -> Option<ExclusionReason> {
    let source = path.as_str();
    if extract.hygiene == Hygiene::Default && is_hygiene_path(source) {
        return Some(ExclusionReason::Hygiene);
    }
    if let Some(pattern) = extract
        .deny
        .iter()
        .find(|pattern| matches_glob(pattern, source))
    {
        return Some(ExclusionReason::Denied {
            pattern: pattern.clone(),
        });
    }
    if let Some(pattern) = extract
        .quarantine
        .iter()
        .find(|pattern| matches_glob(pattern, source))
    {
        return Some(ExclusionReason::Quarantined {
            pattern: pattern.clone(),
        });
    }
    let allowed = extract.allow.is_empty()
        || extract
            .allow
            .iter()
            .any(|pattern| matches_glob(pattern, source));
    (!allowed).then_some(ExclusionReason::NotAllowed)
}

fn is_hygiene_path(path: &str) -> bool {
    let components: Vec<&str> = path.split('/').collect();
    let Some(file_name) = components.last() else {
        return false;
    };
    let debug_symbol = match file_name.rsplit_once('.') {
        Some((_, extension)) => {
            extension.eq_ignore_ascii_case("pdb") || extension.eq_ignore_ascii_case("map")
        }
        None => false,
    };
    matches!(*file_name, ".DS_Store" | "Thumbs.db" | "desktop.ini")
        || debug_symbol
        || components
            .iter()
            .any(|component| matches!(*component, "__MACOSX" | ".git" | ".hg" | ".svn"))
}

fn matches_glob(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let path: Vec<&str> = path.split('/').collect();
    matches_glob_parts(&pattern, &path)
}

fn matches_glob_parts(pattern: &[&str], path: &[&str]) -> bool {
    let Some((head, tail)) = pattern.split_first() else {
        return path.is_empty();
    };
    if *head == "**" {
        if matches_glob_parts(tail, path) {
            return true;
        }
        return match path.split_first() {
            Some((_, rest)) => matches_glob_parts(pattern, rest),
            None => false,
        };
    }
    let Some((path_head, path_tail)) = path.split_first() else {
        return false;
    };
    glob_segment_matches(head, path_head) && matches_glob_parts(tail, path_tail)
}

fn glob_segment_matches(pattern: &str, path: &str) -> bool {
    glob_chars_match(pattern.chars(), path.chars())
}

fn glob_chars_match(pattern: std::str::Chars<'_>, mut path: std::str::Chars<'_>) -> bool {
    let mut rest = pattern.clone();
    match rest.next() {
        None => path.next().is_none(),
        // `*` matches nothing, or consumes one character and stays in effect. The retry must
        // use `pattern`, which still starts with the star, not `rest`.
        Some('*') => {
            glob_chars_match(rest, path.clone())
                || (path.next().is_some() && glob_chars_match(pattern, path))
        }
        Some('?') => path.next().is_some() && glob_chars_match(rest, path),
        Some(character) => path.next() == Some(character) && glob_chars_match(rest, path),
    }
}

/// A failure while resolving a plan into inert filesystem operations.
#[derive(Debug, Error)]
pub enum ResolveError {
    /// The plan violates its schema invariants.
    #[error(transparent)]
    InvalidPlan(#[from] ValidationError),
    /// The requested loader is not declared by the plan.
    #[error("plan does not declare loader {0:?}")]
    UnknownLoader(String),
    /// A placement step uses a malformed loader target reference.
    #[error("invalid deployment target reference {0:?}")]
    InvalidTargetReference(String),
    /// A placement step names a target the selected loader does not declare.
    #[error("loader {loader:?} does not declare target {target:?}")]
    UnknownTarget {
        /// The selected loader.
        loader: String,
        /// The missing target name.
        target: String,
    },
    /// A routed source did not start with its declared stripped prefix.
    #[error("source {path} is not under placement prefix {prefix:?}")]
    OutsidePlacementPrefix {
        /// The artifact-relative source path.
        path: RelPath,
        /// The required leading directory.
        prefix: String,
    },
    /// A template uses `{game_version}`, and the instance has no game version.
    #[error(
        "the plan's {0:?} needs the instance's game version; set one with `msbe instance set <instance> --game-version <version>`"
    )]
    GameVersionRequired(String),
    /// A file a plan derives from is not valid JSON.
    #[error("{path} is not valid JSON: {reason}")]
    InvalidJson {
        /// The instance-relative file.
        path: RelPath,
        /// The parser's message.
        reason: String,
    },
    /// A JSON pointer to set has no object at its parent.
    #[error("{path} has no object to hold JSON pointer {pointer:?}")]
    JsonPointer {
        /// The instance-relative file.
        path: RelPath,
        /// The pointer.
        pointer: String,
    },
    /// Constructing a final instance-relative destination failed.
    #[error(transparent)]
    InvalidPath(#[from] msbe_fsops::Error),
}

#[cfg(test)]
mod tests {
    use msbe_fsops::{Digest, Operation, RelPath};
    use msbe_plan_schema::{
        Deploy, ExtractStep, Hygiene, InjectStep, Loader, NamedPath, PlaceStep, Plan,
        SCHEMA_VERSION, Side, Step,
    };

    use super::{
        ExclusionReason, JsonEdit, ResolveError, ResolvedFile, derive, edit_json, matches_glob,
        resolve,
    };

    fn plan(flatten: bool) -> Plan {
        Plan {
            schema: SCHEMA_VERSION,
            id: "example".to_owned(),
            name: "Example".to_owned(),
            version: "1.0.0".to_owned(),
            deploy: Deploy::default(),
            loaders: vec![Loader {
                id: "loader".to_owned(),
                provides: Vec::new(),
                bootstrap: "none".to_owned(),
                targets: vec![NamedPath {
                    name: "mods".to_owned(),
                    path: "mods".to_owned(),
                }],
                sides: vec![Side::Client],
            }],
            components: Vec::new(),
            steps: vec![Step::Place(PlaceStep {
                loaders: Vec::new(),
                into: "@loader.targets.mods".to_owned(),
                include: Vec::new(),
                exclude: Vec::new(),
                strip_prefix: None,
                flatten,
            })],
        }
    }

    fn file(path: &str) -> ResolvedFile {
        ResolvedFile {
            source: RelPath::new(path).unwrap(),
            blob: Digest::of_bytes(b"mod"),
            mutable: false,
        }
    }

    /// `plan(true)` plus a `container` loader whose steps inject into a game jar.
    fn container_plan() -> Plan {
        let mut plan = plan(true);
        plan.loaders.push(Loader {
            id: "container".to_owned(),
            provides: Vec::new(),
            bootstrap: "none".to_owned(),
            targets: Vec::new(),
            sides: vec![Side::Client],
        });
        if let Some(Step::Place(place)) = plan.steps.first_mut() {
            place.loaders = vec!["loader".to_owned()];
        }
        plan.steps.push(Step::Extract(ExtractStep {
            loaders: vec!["container".to_owned()],
            allow: Vec::new(),
            deny: Vec::new(),
            quarantine: Vec::new(),
            hygiene: Hygiene::Default,
        }));
        plan.steps.push(Step::Inject(InjectStep {
            loaders: vec!["container".to_owned()],
            base: "game/{game_version}.jar".to_owned(),
            into: "game/{game_version}-modded.jar".to_owned(),
            remove: vec!["META-INF/**".to_owned()],
            include: vec!["**/*.class".to_owned()],
        }));
        plan
    }

    #[test]
    fn resolves_a_flattened_file_to_the_selected_loader_target() {
        let operations = resolve(&plan(true), "loader", &[file("release/example.jar")]).unwrap();
        let [Operation::Materialize { path, .. }] = operations.operations.as_slice() else {
            panic!("expected one materialize operation");
        };
        assert_eq!(path.as_str(), "mods/example.jar");
    }

    #[test]
    fn preserves_tree_structure_when_flattening_is_disabled() {
        let operations = resolve(&plan(false), "loader", &[file("release/example.jar")]).unwrap();
        let [Operation::Materialize { path, .. }] = operations.operations.as_slice() else {
            panic!("expected one materialize operation");
        };
        assert_eq!(path.as_str(), "mods/release/example.jar");
    }

    #[test]
    fn plan_declared_mutable_globs_mark_placed_files_by_their_destination() {
        let mut plan = plan(true);
        plan.deploy.mutable = vec!["@loader.targets.mods/*.cfg".to_owned()];
        let resolved = resolve(
            &plan,
            "loader",
            &[file("release/example.jar"), file("release/settings.cfg")],
        )
        .unwrap();
        let flags: Vec<(&str, bool)> = resolved
            .operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::Materialize { path, mutable, .. } => Some((path.as_str(), *mutable)),
                _ => None,
            })
            .collect();
        assert_eq!(
            flags,
            [("mods/example.jar", false), ("mods/settings.cfg", true)]
        );
    }

    #[test]
    fn rejects_a_loader_that_the_plan_does_not_declare() {
        let error = resolve(&plan(true), "other", &[]).unwrap_err();
        assert!(matches!(error, ResolveError::UnknownLoader(loader) if loader == "other"));
    }

    #[test]
    fn excludes_hygiene_and_quarantined_files_from_the_deployment_report() {
        let mut plan = plan(true);
        plan.steps.clear();
        plan.steps.push(Step::Extract(ExtractStep {
            loaders: Vec::new(),
            allow: vec!["**/*.jar".to_owned()],
            deny: Vec::new(),
            quarantine: vec!["Docs/**".to_owned()],
            hygiene: Hygiene::Default,
        }));
        plan.steps.push(Step::Place(PlaceStep {
            loaders: Vec::new(),
            into: "@loader.targets.mods".to_owned(),
            include: Vec::new(),
            exclude: Vec::new(),
            strip_prefix: None,
            flatten: true,
        }));

        let resolved = resolve(
            &plan,
            "loader",
            &[
                file("release/example.jar"),
                file(".git/config"),
                file("Docs/README.md"),
                file("symbols/EXAMPLE.PDB"),
            ],
        )
        .unwrap();

        assert_eq!(resolved.operations.len(), 1);
        assert!(
            resolved
                .excluded
                .iter()
                .any(|file| file.source.as_str() == ".git/config"
                    && file.reason == ExclusionReason::Hygiene)
        );
        assert!(resolved.excluded.iter().any(|file| {
            file.source.as_str() == "Docs/README.md"
                && file.reason
                    == ExclusionReason::Quarantined {
                        pattern: "Docs/**".to_owned(),
                    }
        }));
        assert!(
            resolved
                .excluded
                .iter()
                .any(|file| file.source.as_str() == "symbols/EXAMPLE.PDB"
                    && file.reason == ExclusionReason::Hygiene)
        );
    }

    #[test]
    fn an_explicit_deny_is_reported_over_a_missing_allow_match() {
        let mut plan = plan(true);
        plan.steps.insert(
            0,
            Step::Extract(ExtractStep {
                loaders: Vec::new(),
                allow: vec!["**/*.jar".to_owned()],
                deny: vec!["**/*.exe".to_owned()],
                quarantine: Vec::new(),
                hygiene: Hygiene::Default,
            }),
        );
        let resolved = resolve(&plan, "loader", &[file("tools/installer.exe")]).unwrap();
        assert_eq!(
            resolved.excluded.first().map(|excluded| &excluded.reason),
            Some(&ExclusionReason::Denied {
                pattern: "**/*.exe".to_owned()
            })
        );
    }

    #[test]
    fn steps_limited_to_another_loader_neither_place_nor_filter() {
        let plan = container_plan();
        let placed = resolve(&plan, "loader", &[file("a/B.class")]).unwrap();
        assert_eq!(placed.operations.len(), 1);
        assert!(placed.excluded.is_empty());

        let contained = resolve(&plan, "container", &[file("__MACOSX/._B.class")]).unwrap();
        assert!(contained.operations.is_empty());
        assert_eq!(
            contained.excluded.first().map(|excluded| &excluded.reason),
            Some(&ExclusionReason::Hygiene)
        );
        assert_eq!(
            derive(&plan, "loader", Some("1.5.2"), &[vec![file("a/B.class")]]).unwrap(),
            super::Derivations::default()
        );
    }

    #[test]
    fn injected_entries_follow_profile_order_and_templates_take_the_game_version() {
        let plan = container_plan();
        let first = vec![file("a/B.class"), file("readme.txt")];
        let second = vec![file("a/B.class"), file("__MACOSX/._a"), file("c/D.class")];
        let derived = derive(&plan, "container", Some("1.5.2"), &[first, second]).unwrap();
        let [injection] = derived.injections.as_slice() else {
            panic!("expected one injection, got {derived:?}");
        };
        assert_eq!(injection.base.as_str(), "game/1.5.2.jar");
        assert_eq!(injection.into.as_str(), "game/1.5.2-modded.jar");
        let entries: Vec<&str> = injection
            .entries
            .iter()
            .map(|entry| entry.source.as_str())
            .collect();
        assert_eq!(entries, ["a/B.class", "a/B.class", "c/D.class"]);
        assert!(injection.removes("META-INF/MOJANG_C.SF"));
        assert!(injection.removes("META-INF"));
        assert!(!injection.removes("a/B.class"));

        assert!(matches!(
            derive(&plan, "container", None, &[]),
            Err(ResolveError::GameVersionRequired(_))
        ));
    }

    #[test]
    fn json_edits_remove_then_set_pointers_and_refuse_a_missing_parent() {
        let edit = |set: &[(&str, &str)], remove: &[&str]| JsonEdit {
            base: RelPath::new("versions/1.5.2/1.5.2.json").unwrap(),
            into: RelPath::new("versions/1.5.2-msbe/1.5.2-msbe.json").unwrap(),
            set: set
                .iter()
                .map(|(pointer, value)| ((*pointer).to_owned(), (*value).to_owned()))
                .collect(),
            remove: remove.iter().map(|pointer| (*pointer).to_owned()).collect(),
        };
        let vanilla =
            br#"{"id":"1.5.2","downloads":{"client":{"sha1":"x"},"server":{"sha1":"y"}},"a/b":1}"#;
        let edited = edit_json(
            vanilla,
            &edit(
                &[("/id", "1.5.2-msbe")],
                &["/downloads/client", "/absent/entirely", "/a~1b"],
            ),
        )
        .unwrap();
        let document: serde_json::Value = serde_json::from_slice(&edited).unwrap();
        assert_eq!(
            document,
            serde_json::json!({ "id": "1.5.2-msbe", "downloads": { "server": { "sha1": "y" } } })
        );
        assert!(edited.ends_with(b"\n"));

        assert!(matches!(
            edit_json(vanilla, &edit(&[("/missing/id", "x")], &[])),
            Err(ResolveError::JsonPointer { .. })
        ));
        assert!(matches!(
            edit_json(b"not json", &edit(&[], &[])),
            Err(ResolveError::InvalidJson { .. })
        ));
    }

    #[test]
    fn a_star_matches_any_run_of_characters_within_one_component() {
        assert!(matches_glob("*.jar", "example.jar"));
        assert!(matches_glob("*", "anything"));
        assert!(matches_glob("a*b*c", "aXXbYYc"));
        assert!(!matches_glob("a*b*c", "aXXbYY"));
        assert!(matches_glob("?at.txt", "cat.txt"));
        assert!(!matches_glob("*.jar", "example.jar.disabled"));
        assert!(matches_glob("**", "any/depth/at/all"));
        assert!(matches_glob("**/*.jar", "top-level.jar"));
    }

    #[test]
    fn glob_patterns_support_recursive_directories_and_component_wildcards() {
        assert!(matches_glob("**/*.jar", "release/nested/example.jar"));
        assert!(matches_glob("mods/*.jar", "mods/example.jar"));
        assert!(!matches_glob("mods/*.jar", "mods/nested/example.jar"));
    }
}
