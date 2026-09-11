//! Plan engine, solver and the resolve/apply pipeline.
//!
//! Knows nothing about any game. If a game name, store name, loader or file format
//! appears in this crate, that is a design bug: the test is that this crate compiles
//! and passes its suite with zero plans installed.
//!
//! See `docs/00-overview.md` and `docs/02-plan-system.md`.

pub mod config;
pub mod instance;

use msbe_fsops::{Applier, Digest, Observer, Operation, RelPath, Result as FsResult, TxnReport};
use msbe_plan_schema::{ExtractStep, Hygiene, Loader, Plan, Step, ValidationError};
use serde::{Deserialize, Serialize};
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
/// transactional applier unchanged.
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

    let (files, excluded) = filter_files(plan, files);
    let mutable_globs = mutable_globs(plan, loader);
    let mut operations = Vec::new();
    for step in &plan.steps {
        let Step::Place(place) = step else {
            continue;
        };
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
            let source = if place.flatten {
                file.source.file_name()
            } else {
                file.source.as_str()
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

fn filter_files<'a>(
    plan: &Plan,
    files: &'a [ResolvedFile],
) -> (Vec<&'a ResolvedFile>, Vec<ExcludedFile>) {
    let extract_steps: Vec<&ExtractStep> = plan
        .steps
        .iter()
        .filter_map(|step| match step {
            Step::Extract(extract) => Some(extract),
            Step::Place(_) => None,
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
    /// Constructing a final instance-relative destination failed.
    #[error(transparent)]
    InvalidPath(#[from] msbe_fsops::Error),
}

#[cfg(test)]
mod tests {
    use msbe_fsops::{Digest, Operation, RelPath};
    use msbe_plan_schema::{
        Deploy, ExtractStep, Hygiene, Loader, NamedPath, PlaceStep, Plan, SCHEMA_VERSION, Side,
        Step,
    };

    use super::{ExclusionReason, ResolveError, ResolvedFile, matches_glob, resolve};

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
            steps: vec![Step::Place(PlaceStep {
                into: "@loader.targets.mods".to_owned(),
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
            allow: vec!["**/*.jar".to_owned()],
            deny: Vec::new(),
            quarantine: vec!["Docs/**".to_owned()],
            hygiene: Hygiene::Default,
        }));
        plan.steps.push(Step::Place(PlaceStep {
            into: "@loader.targets.mods".to_owned(),
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
