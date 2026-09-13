//! Opening a pack, staging the layer it describes, and obtaining its bytes.
//!
//! Import and update share this path (§17.12): the host opens the input under its limits, a codec
//! plans it, installation inputs are verified before anything is acquired, and every requirement is
//! obtained through its sources in preference order and verified before the store admits it.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, BufReader},
    path::{Path, PathBuf},
};

use msbe_archive::Limits;
use msbe_core::instance::{
    ComponentEntry, Instance, LockedTarget, Lockfile, ModEntry, Name, PackFileRole, Profile,
    ProfileLayer, ProfileTarget, Provenance, StoredFile,
};
use msbe_fsops::{Digest, RelPath};
use msbe_provider_api::{
    EnvironmentRequirement, HttpClient, PackImportContext, PackImportPlan, PackOptions,
    PackRequirement, PackWarning, PackageId, RequirementSource, Target,
    model::{ReleaseFile, Request, Selection},
};
use msbe_providers::Providers;
use serde::{Deserialize, Serialize};

use crate::{
    Connect, IssueCode, PackError, PackIssue, Progress,
    catalog::{self, Direction},
    error::io_error,
    files,
    host::ZipPackInput,
    progress::checkpoint,
};

/// The environment root that names an instance's game directory.
pub(crate) const GAME_ROOT: &str = "game";

/// How one item of a staged layer will be obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImportAction {
    /// The store already holds it.
    InStore,
    /// The input embeds it; the host ingests and verifies it.
    Embedded,
    /// It is acquired through its sources.
    Acquire,
    /// Only user action can supply it.
    UserAction,
    /// An unchanged module is kept from the current profile.
    Reuse,
    /// Deployment regenerates it.
    Derive,
    /// Nothing supplies it.
    Missing,
}

/// One item a preview says will be obtained, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportItem {
    /// A module name, or the path of a pack-owned or derived file.
    pub subject: String,
    /// The exact digest, when the pack pins one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<Digest>,
    /// How it will be obtained.
    pub action: ImportAction,
    /// Where it may be obtained, in preference order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<RequirementSource>,
}

/// One module a staged layer supplies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StagedModule {
    /// Exact stored files already known, from a native lockfile or a reused module.
    Exact {
        /// The module's files and provenance.
        entry: ModEntry,
    },
    /// One file acquired through a requirement and routed to its destination.
    Acquire {
        /// The requirement to satisfy.
        requirement: PackRequirement,
    },
}

/// The layer an input describes, before any bytes are obtained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedLayer {
    /// The compatibility target the layer resolves against.
    pub target: ProfileTarget,
    /// Module order.
    pub order: Vec<Name>,
    /// Modules by name.
    pub mods: BTreeMap<Name, StagedModule>,
    /// Pack-owned files.
    pub configs: BTreeMap<RelPath, Digest>,
    /// Loader components.
    pub components: BTreeMap<String, ComponentEntry>,
    /// Profile lineage a native lockfile preserves.
    pub layers: Vec<ProfileLayer>,
    /// The deployment digest map a native lockfile requires the result to reach.
    pub deployment: Option<BTreeMap<RelPath, Digest>>,
}

/// A staged layer and what a preview reports about it.
#[derive(Debug)]
pub(crate) struct Staged {
    pub(crate) layer: StagedLayer,
    pub(crate) items: Vec<ImportItem>,
    pub(crate) blockers: Vec<PackIssue>,
    pub(crate) warnings: Vec<PackWarning>,
}

/// An opened input and the plan its codec produced.
#[derive(Debug)]
pub(crate) struct Opened {
    pub(crate) input: ZipPackInput,
    pub(crate) plan: PackImportPlan,
}

/// `instance`'s default compatibility target.
pub(crate) fn default_target(instance: &Instance) -> ProfileTarget {
    let config = instance.config();
    ProfileTarget {
        loader: config.loader.clone(),
        loader_version: config.loader_version.clone(),
        side: config.side,
    }
}

/// Opens `path` under host limits and plans it with the requested codec, or the one that
/// recognizes it.
pub(crate) fn open(
    providers: &Providers,
    instance: &Instance,
    target: &ProfileTarget,
    path: &Path,
    codec: Option<&str>,
    options: &PackOptions,
) -> Result<Opened, PackError> {
    let input = ZipPackInput::open(path, &Limits::default())?;
    let snapshot = RelPath::new(files::SNAPSHOT_MANIFEST)
        .map_err(|error| PackError::issue(IssueCode::HostFailure, error.to_string()))?;
    if input.contains(&snapshot) {
        return Err(PackError::issue(
            IssueCode::UnknownCodec,
            "this is an instance snapshot, not a pack; restore it with snapshot restore",
        ));
    }
    let codec = match codec {
        Some(id) => {
            catalog::descriptor(providers, id, Direction::Import)?;
            providers.pack_codec(id)?
        }
        None => providers.detect_pack_codec(&input)?.ok_or_else(|| {
            PackError::issue(
                IssueCode::UnknownCodec,
                format!("no reviewed codec recognizes {}", path.display()),
            )
        })?,
    };
    if !codec.descriptor().directions.import {
        return Err(PackError::issue(
            IssueCode::UnsupportedDirection,
            format!("codec {} does not import", codec.descriptor().id),
        ));
    }
    let options = codec.descriptor().option_schema.normalize(options)?;
    let context = PackImportContext {
        game: Some(instance.plan().id.clone()),
        target: Some(LockedTarget {
            game_version: instance.config().game_version.clone(),
            edition: instance.config().edition.clone(),
            storefront: instance.config().storefront.clone(),
            loader: target.loader.clone(),
            loader_version: target.loader_version.clone(),
            side: target.side,
            fingerprint: None,
        }),
    };
    let mut plan = codec.plan_import(&input, &context, &options)?;
    plan.origin.digest = input.digest();
    Ok(Opened { input, plan })
}

/// Stages the layer `plan` describes for `instance`, verifying installation inputs first.
/// Modules in `reuse` whose content identity matches are kept rather than acquired again.
pub(crate) fn stage(
    instance: &Instance,
    plan: &PackImportPlan,
    target: &ProfileTarget,
    reuse: &BTreeMap<Name, ModEntry>,
) -> Result<Staged, PackError> {
    let mut staged = Staged {
        layer: StagedLayer {
            target: target.clone(),
            order: Vec::new(),
            mods: BTreeMap::new(),
            configs: BTreeMap::new(),
            components: BTreeMap::new(),
            layers: Vec::new(),
            deployment: None,
        },
        items: Vec::new(),
        blockers: Vec::new(),
        warnings: plan.warnings.clone(),
    };
    if let Some(game) = &plan.target.game
        && *game != instance.plan().id
    {
        staged.blockers.push(PackIssue::new(
            IssueCode::UnsupportedTarget,
            format!(
                "the pack targets {game}, but this instance uses {}",
                instance.plan().id
            ),
        ));
    }
    let config = instance.config();
    for (kind, declared, installed) in [
        ("edition", &plan.target.edition, &config.edition),
        ("storefront", &plan.target.storefront, &config.storefront),
    ] {
        if let Some(declared) = declared
            && installed.as_ref() != Some(declared)
        {
            staged.blockers.push(PackIssue::new(
                IssueCode::UnsupportedTarget,
                format!(
                    "the pack targets {kind} {declared}, but this instance's {kind} is {}",
                    installed.as_deref().unwrap_or("not set")
                ),
            ));
        }
    }
    verify_environment(instance, &plan.environment, &mut staged.blockers)?;
    match &plan.lockfile {
        Some(lockfile) => stage_lockfile(instance, plan, lockfile, &mut staged)?,
        None => stage_requirements(instance, plan, reuse, &mut staged)?,
    }
    Ok(staged)
}

fn verify_environment(
    instance: &Instance,
    environment: &[EnvironmentRequirement],
    blockers: &mut Vec<PackIssue>,
) -> Result<(), PackError> {
    for requirement in environment {
        let found = if requirement.root == GAME_ROOT {
            digest_of(&requirement.path.to_path(&instance.config().root))?
        } else {
            None
        };
        if found != Some(requirement.digest) {
            blockers.push(environment_mismatch(
                &requirement.path,
                requirement.digest,
                found,
            ));
        }
    }
    Ok(())
}

fn environment_mismatch(path: &RelPath, expected: Digest, found: Option<Digest>) -> PackIssue {
    PackIssue {
        code: IssueCode::EnvironmentMismatch,
        path: Some(path.clone()),
        digest: Some(expected),
        message: format!(
            "{path} is {} in this installation, but the pack was built against {expected}",
            found.map_or_else(|| "missing".to_owned(), |digest| digest.to_string())
        ),
    }
}

fn stage_lockfile(
    instance: &Instance,
    plan: &PackImportPlan,
    lockfile: &Lockfile,
    staged: &mut Staged,
) -> Result<(), PackError> {
    if let Some(fingerprint) = &lockfile.target.fingerprint {
        for (path, digest) in &fingerprint.identifying {
            let found = digest_of(&path.to_path(&instance.config().root))?;
            if found != Some(*digest) {
                staged
                    .blockers
                    .push(environment_mismatch(path, *digest, found));
            }
        }
    }
    let layer = &mut staged.layer;
    layer.target = ProfileTarget {
        loader: lockfile.target.loader.clone(),
        loader_version: lockfile.target.loader_version.clone(),
        side: lockfile.target.side,
    };
    if !instance
        .plan()
        .loaders
        .iter()
        .any(|loader| loader.id == layer.target.loader)
    {
        staged.blockers.push(PackIssue::new(
            IssueCode::UnsupportedTarget,
            format!(
                "this instance's plan does not declare loader {}",
                layer.target.loader
            ),
        ));
    }
    layer.order.clone_from(&lockfile.order);
    layer.mods = lockfile
        .mods
        .iter()
        .map(|(name, module)| {
            let entry = ModEntry {
                origin: module.origin.clone(),
                provider: module.provider.clone(),
                files: module.files.clone(),
                answers: module.answers.clone(),
            };
            (name.clone(), StagedModule::Exact { entry })
        })
        .collect();
    layer.configs = lockfile.configs();
    layer.components.clone_from(&lockfile.components);
    layer.layers.clone_from(&lockfile.layers);
    layer.deployment = Some(lockfile.deployment.clone());

    let embedded: BTreeSet<Digest> = plan.embedded.iter().map(|blob| blob.digest).collect();
    for digest in lockfile.required_blobs() {
        let requirement = plan
            .requirements
            .iter()
            .find(|requirement| requirement.digest == Some(digest));
        let (action, sources) = if instance.store().contains(&digest) {
            (ImportAction::InStore, Vec::new())
        } else if embedded.contains(&digest) {
            (ImportAction::Embedded, Vec::new())
        } else if let Some(requirement) = requirement {
            (acquisition(requirement), requirement.sources.clone())
        } else {
            staged.blockers.push(PackIssue {
                code: IssueCode::MissingBlob,
                path: None,
                digest: Some(digest),
                message: format!("the bundle neither embeds nor sources {digest}"),
            });
            (ImportAction::Missing, Vec::new())
        };
        staged.items.push(ImportItem {
            subject: subject(lockfile, &digest),
            digest: Some(digest),
            action,
            sources,
        });
    }
    for (path, digest) in &lockfile.deployment {
        let generated = lockfile
            .classifications
            .get(path)
            .is_some_and(|classification| classification.role == PackFileRole::Generated);
        if generated {
            staged.items.push(ImportItem {
                subject: path.to_string(),
                digest: Some(*digest),
                action: ImportAction::Derive,
                sources: Vec::new(),
            });
        }
    }
    Ok(())
}

fn stage_requirements(
    instance: &Instance,
    plan: &PackImportPlan,
    reuse: &BTreeMap<Name, ModEntry>,
    staged: &mut Staged,
) -> Result<(), PackError> {
    for requirement in &plan.requirements {
        let Some(destination) = &requirement.destination else {
            staged.blockers.push(PackIssue::new(
                IssueCode::UnreproducibleContent,
                "a pack requirement has no destination path",
            ));
            continue;
        };
        if requirement_identity(requirement).is_empty() {
            staged.blockers.push(PackIssue {
                code: IssueCode::MissingDigest,
                path: Some(destination.clone()),
                digest: None,
                message: format!("{destination} does not pin its bytes with a digest or hash"),
            });
            continue;
        }
        let name = module_name(destination)?;
        if staged.layer.mods.contains_key(&name) {
            staged.blockers.push(PackIssue {
                code: IssueCode::LayerConflict,
                path: Some(destination.clone()),
                digest: None,
                message: format!("more than one pack file would become module {name}"),
            });
            continue;
        }
        let reused = reuse
            .get(&name)
            .filter(|entry| same(&entry_identity(entry), &requirement_identity(requirement)));
        let (module, action) = match reused {
            Some(entry) => (
                StagedModule::Exact {
                    entry: entry.clone(),
                },
                ImportAction::Reuse,
            ),
            None if requirement.sources.is_empty() => {
                staged.blockers.push(PackIssue {
                    code: IssueCode::MissingBlob,
                    path: Some(destination.clone()),
                    digest: requirement.digest,
                    message: format!("{destination} has no source to obtain it from"),
                });
                continue;
            }
            None => (
                StagedModule::Acquire {
                    requirement: requirement.clone(),
                },
                acquisition(requirement),
            ),
        };
        staged.items.push(ImportItem {
            subject: name.to_string(),
            digest: requirement.digest,
            action,
            sources: requirement.sources.clone(),
        });
        staged.layer.order.push(name.clone());
        staged.layer.mods.insert(name, module);
    }
    for blob in &plan.embedded {
        let Some(destination) = &blob.destination else {
            staged.warnings.push(PackWarning {
                code: "ignored-entry".to_owned(),
                message: format!("{} has no destination and is not imported", blob.entry),
            });
            continue;
        };
        staged
            .layer
            .configs
            .insert(destination.clone(), blob.digest);
        staged.items.push(ImportItem {
            subject: destination.to_string(),
            digest: Some(blob.digest),
            action: if instance.store().contains(&blob.digest) {
                ImportAction::InStore
            } else {
                ImportAction::Embedded
            },
            sources: Vec::new(),
        });
    }
    Ok(())
}

fn acquisition(requirement: &PackRequirement) -> ImportAction {
    if requirement
        .sources
        .iter()
        .all(|source| matches!(source, RequirementSource::UserAction { .. }))
    {
        ImportAction::UserAction
    } else {
        ImportAction::Acquire
    }
}

/// A readable subject for `digest`: its first deployment path, else the artifact file holding it.
fn subject(lockfile: &Lockfile, digest: &Digest) -> String {
    lockfile
        .deployment
        .iter()
        .find(|(_, found)| *found == digest)
        .map(|(path, _)| path.to_string())
        .or_else(|| {
            lockfile
                .mods
                .values()
                .flat_map(|module| &module.files)
                .find(|file| file.blob == *digest)
                .map(|file| file.source.to_string())
        })
        .unwrap_or_else(|| digest.to_string())
}

/// The module name a pack file becomes: its file stem, made safe.
fn module_name(destination: &RelPath) -> Result<Name, PackError> {
    let file = destination.file_name();
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    Ok(Name::sanitize(stem)?)
}

/// The content identity of a module or requirement, by the strongest facts available.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Identity {
    sha512: Option<String>,
    sha256: Option<String>,
    blobs: BTreeSet<Digest>,
}

impl Identity {
    fn is_empty(&self) -> bool {
        self.sha512.is_none() && self.sha256.is_none() && self.blobs.is_empty()
    }
}

/// The identity of a stored module.
pub(crate) fn entry_identity(entry: &ModEntry) -> Identity {
    let hash = |algorithm: &str| {
        entry
            .provider
            .as_ref()
            .and_then(|provenance| provenance.hashes.get(algorithm))
            .map(|hash| hash.to_ascii_lowercase())
    };
    Identity {
        sha512: hash("sha512"),
        sha256: hash("sha256"),
        blobs: entry.files.iter().map(|file| file.blob).collect(),
    }
}

/// The identity a requirement pins.
pub(crate) fn requirement_identity(requirement: &PackRequirement) -> Identity {
    let hash = |algorithm: &str| {
        requirement
            .hashes
            .get(algorithm)
            .map(|hash| hash.to_ascii_lowercase())
    };
    Identity {
        sha512: hash("sha512"),
        sha256: hash("sha256").or_else(|| {
            requirement.digest.and_then(|digest| {
                digest
                    .to_string()
                    .strip_prefix("sha256:")
                    .map(str::to_owned)
            })
        }),
        blobs: requirement.digest.into_iter().collect(),
    }
}

/// The identity of a staged module.
pub(crate) fn module_identity(module: &StagedModule) -> Identity {
    match module {
        StagedModule::Exact { entry } => entry_identity(entry),
        StagedModule::Acquire { requirement } => requirement_identity(requirement),
    }
}

/// Whether two identities name the same bytes. Unknown identity is never the same.
pub(crate) fn same(left: &Identity, right: &Identity) -> bool {
    match (&left.sha512, &right.sha512, &left.sha256, &right.sha256) {
        (Some(left), Some(right), ..) | (_, _, Some(left), Some(right)) => left == right,
        _ => !left.blobs.is_empty() && left.blobs == right.blobs,
    }
}

/// Assembles a profile from a staged layer and its obtained modules, recording the layer as the
/// profile's pack base unless a native lockfile already carries the lineage.
pub(crate) fn assemble(
    layer: &StagedLayer,
    plan: &PackImportPlan,
    mods: BTreeMap<Name, ModEntry>,
) -> Profile {
    let layers = if layer.layers.is_empty() {
        pack_layers(layer, plan, &mods)
    } else {
        layer.layers.clone()
    };
    Profile {
        target: Some(layer.target.clone()),
        order: layer.order.clone(),
        components: layer.components.clone(),
        mods,
        configs: layer.configs.clone(),
        layers,
    }
}

/// A pack layer holding `mods` as its base, followed by an empty changes layer.
pub(crate) fn pack_layers(
    layer: &StagedLayer,
    plan: &PackImportPlan,
    mods: &BTreeMap<Name, ModEntry>,
) -> Vec<ProfileLayer> {
    vec![
        ProfileLayer {
            id: ProfileLayer::PACK.to_owned(),
            kind: ProfileLayer::PACK.to_owned(),
            codec: Some(plan.codec.clone()),
            pack: plan.origin.pack.clone(),
            version: plan.origin.version.clone(),
            digest: Some(plan.origin.digest),
            mods: mods.clone(),
            configs: layer.configs.clone(),
            order: layer.order.clone(),
        },
        ProfileLayer {
            id: ProfileLayer::CHANGES.to_owned(),
            kind: ProfileLayer::CHANGES.to_owned(),
            codec: None,
            pack: None,
            version: None,
            digest: None,
            mods: BTreeMap::new(),
            configs: BTreeMap::new(),
            order: Vec::new(),
        },
    ]
}

/// Obtains every byte a staged layer needs: embedded blobs through the host, native blobs and
/// modules through their sources. Returns the layer's modules with their stored files. Nothing is
/// written to the profile.
pub(crate) fn obtain(
    fetcher: &mut Fetcher<'_>,
    input: &ZipPackInput,
    plan: &PackImportPlan,
    layer: &StagedLayer,
    progress: &dyn Progress,
) -> Result<BTreeMap<Name, ModEntry>, PackError> {
    let store = fetcher.instance.store();
    let total = u64::try_from(plan.embedded.len() + layer.mods.len()).unwrap_or(u64::MAX);
    let mut done = 0_u64;
    for blob in &plan.embedded {
        checkpoint(progress)?;
        progress.report(done, total, &format!("verifying {}", blob.entry));
        done = done.saturating_add(1);
        if store.contains(&blob.digest) {
            continue;
        }
        let found = input.with_entry(&blob.entry, u64::MAX, |reader| {
            store.put_reader(reader).map_err(io::Error::other)
        })?;
        if found != blob.digest {
            return Err(integrity(&blob.entry.to_string(), blob.digest, found));
        }
    }
    if let Some(lockfile) = &plan.lockfile {
        for digest in lockfile.required_blobs() {
            checkpoint(progress)?;
            if store.contains(&digest) {
                continue;
            }
            let requirement = plan
                .requirements
                .iter()
                .find(|requirement| requirement.digest == Some(digest))
                .ok_or_else(|| missing(digest))?;
            let acquired = fetcher.acquire(requirement)?;
            let stored = store.put_file(&acquired.path)?;
            if stored != digest {
                return Err(integrity(&subject(lockfile, &digest), digest, stored));
            }
        }
    }
    let mut mods = BTreeMap::new();
    for (name, module) in &layer.mods {
        checkpoint(progress)?;
        progress.report(done, total, &format!("obtaining {name}"));
        done = done.saturating_add(1);
        let entry = match module {
            StagedModule::Exact { entry } => {
                if let Some(file) = entry.files.iter().find(|file| !store.contains(&file.blob)) {
                    return Err(missing(file.blob));
                }
                entry.clone()
            }
            StagedModule::Acquire { requirement } => fetcher.module(requirement)?,
        };
        mods.insert(name.clone(), entry);
    }
    let required = layer.configs.values().copied().chain(
        layer
            .components
            .values()
            .flat_map(|component| component.files.iter().map(|file| file.blob)),
    );
    for digest in required {
        if !store.contains(&digest) {
            return Err(missing(digest));
        }
    }
    progress.report(total, total, "obtained");
    Ok(mods)
}

fn missing(digest: Digest) -> PackError {
    PackError::Issue(PackIssue {
        code: IssueCode::MissingBlob,
        path: None,
        digest: Some(digest),
        message: format!("nothing supplies required blob {digest}"),
    })
}

fn integrity(subject: &str, expected: Digest, found: Digest) -> PackError {
    PackError::Issue(PackIssue {
        code: IssueCode::IntegrityMismatch,
        path: None,
        digest: Some(expected),
        message: format!("{subject} should be {expected} but is {found}"),
    })
}

fn digest_of(path: &Path) -> Result<Option<Digest>, PackError> {
    match File::open(path) {
        Ok(file) => Digest::of_reader(BufReader::new(file))
            .map(Some)
            .map_err(io_error("read", path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io_error("open", path)(source)),
    }
}

/// A file obtained for a requirement, and the provenance its provider recorded.
#[derive(Debug)]
pub(crate) struct Acquired {
    path: PathBuf,
    provenance: Option<Provenance>,
}

/// Obtains requirement bytes through the provider registry and its policy gate.
pub(crate) struct Fetcher<'a> {
    providers: &'a Providers,
    instance: &'a Instance,
    target: Target,
    connect: Connect<'a>,
    client: Option<Box<dyn HttpClient>>,
    scratch: tempfile::TempDir,
}

impl std::fmt::Debug for Fetcher<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Fetcher")
            .field("target", &self.target)
            .field("connected", &self.client.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> Fetcher<'a> {
    /// A fetcher downloading into scratch space on `instance`'s store volume.
    pub(crate) fn new(
        providers: &'a Providers,
        instance: &'a Instance,
        target: &ProfileTarget,
        connect: Connect<'a>,
    ) -> Result<Self, PackError> {
        let scratch_dir = instance.scratch_dir();
        std::fs::create_dir_all(&scratch_dir).map_err(io_error("create", &scratch_dir))?;
        let scratch = tempfile::tempdir_in(&scratch_dir)
            .map_err(io_error("create scratch space in", &scratch_dir))?;
        Ok(Self {
            providers,
            instance,
            target: Target {
                game: instance.plan().id.clone(),
                edition: instance.config().edition.clone(),
                storefront: instance.config().storefront.clone(),
                loader: target.loader.clone(),
                provides: instance.target_provides(target),
                loader_version: target.loader_version.clone(),
                game_version: instance.config().game_version.clone(),
                side: target.side,
            },
            connect,
            client: None,
            scratch,
        })
    }

    /// Acquires a requirement as a module whose one file is routed to its destination.
    fn module(&mut self, requirement: &PackRequirement) -> Result<ModEntry, PackError> {
        let destination = requirement.destination.clone().ok_or_else(|| {
            PackError::issue(
                IssueCode::UnreproducibleContent,
                "a pack requirement has no destination path",
            )
        })?;
        let acquired = self.acquire(requirement)?;
        let files = msbe_archive::ingest_as_file(
            self.instance.store(),
            &acquired.path,
            destination,
            &Limits::default(),
        )?;
        Ok(ModEntry {
            origin: acquired
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            provider: acquired.provenance,
            files: files
                .into_iter()
                .map(|file| StoredFile {
                    source: file.source,
                    blob: file.blob,
                })
                .collect(),
            answers: requirement.answers.clone(),
        })
    }

    /// Tries each of `requirement`'s sources in order and returns the first whose bytes match.
    fn acquire(&mut self, requirement: &PackRequirement) -> Result<Acquired, PackError> {
        let mut failures = Vec::new();
        let mut actions = Vec::new();
        for source in &requirement.sources {
            let attempt = match source {
                RequirementSource::Direct { urls } => self.urls(urls, requirement),
                RequirementSource::Provider { package, version } => {
                    self.release(package, version.as_deref(), requirement)
                }
                RequirementSource::UserAction {
                    provider,
                    reference,
                    reason,
                } => {
                    actions.push(format!("{provider} {reference}: {reason}"));
                    continue;
                }
            };
            match attempt.and_then(|acquired| verified(requirement, acquired)) {
                Ok(acquired) => return Ok(acquired),
                Err(error) => failures.push(error.to_string()),
            }
        }
        let subject = requirement
            .destination
            .as_ref()
            .map_or_else(|| format!("{:?}", requirement.digest), ToString::to_string);
        if actions.is_empty() {
            Err(PackError::Issue(PackIssue {
                code: IssueCode::MissingBlob,
                path: requirement.destination.clone(),
                digest: requirement.digest,
                message: format!("no source supplied {subject}: {}", failures.join("; ")),
            }))
        } else {
            Err(PackError::Issue(PackIssue {
                code: IssueCode::UserActionRequired,
                path: requirement.destination.clone(),
                digest: requirement.digest,
                message: format!("{subject} needs user action: {}", actions.join("; ")),
            }))
        }
    }

    fn connected(&mut self) -> Result<(), PackError> {
        if self.client.is_none() {
            self.client = Some((self.connect)()?);
        }
        Ok(())
    }

    fn urls(
        &mut self,
        urls: &[String],
        requirement: &PackRequirement,
    ) -> Result<Acquired, PackError> {
        let mut last = PackError::issue(IssueCode::MissingBlob, "no HTTPS download is listed");
        for url in urls.iter().filter(|url| url.starts_with("https://")) {
            match self.routed(&pin(url, requirement), None) {
                Ok(acquired) => return Ok(acquired),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    fn release(
        &mut self,
        package: &PackageId,
        version: Option<&str>,
        requirement: &PackRequirement,
    ) -> Result<Acquired, PackError> {
        self.connected()?;
        let adapter = self.providers.adapter(&package.provider)?;
        let http = self.client.as_deref().ok_or_else(|| {
            PackError::issue(IssueCode::HostFailure, "no network client is available")
        })?;
        if let (Some(releases), Some(version)) = (adapter.as_releases(), version) {
            let release = releases
                .releases(http, &package.project, &self.target)?
                .into_iter()
                .find(|release| release.id == version)
                .ok_or_else(|| {
                    PackError::issue(
                        IssueCode::MissingExactVersion,
                        format!(
                            "{} no longer lists release {version} of {}",
                            package.provider, package.project
                        ),
                    )
                })?;
            let file = release
                .files
                .iter()
                .find(|file| matches_hashes(file, requirement))
                .or_else(|| release.files.iter().find(|file| file.primary))
                .cloned()
                .ok_or_else(|| {
                    PackError::issue(
                        IssueCode::MissingExactVersion,
                        format!(
                            "release {version} of {} has no matching file",
                            package.project
                        ),
                    )
                })?;
            let acquired = adapter.acquire(http, &file, self.scratch.path())?;
            return Ok(Acquired {
                path: acquired.path.clone(),
                provenance: Some(adapter.provenance(&release, &acquired)),
            });
        }
        // A provider whose project reference is itself a routable source, such as a pinned URL.
        self.routed(&pin(&package.project, requirement), Some(&package.provider))
    }

    /// Routes `raw` through the registry and acquires the one file it names.
    fn routed(&mut self, raw: &str, provider: Option<&str>) -> Result<Acquired, PackError> {
        self.connected()?;
        let routed = self.providers.request(raw)?;
        if provider.is_some_and(|expected| expected != routed.provider) {
            return Err(PackError::issue(
                IssueCode::MissingExactVersion,
                format!("{raw} is not served by {}", provider.unwrap_or_default()),
            ));
        }
        let Request::File(selection) = routed.request else {
            return Err(PackError::issue(
                IssueCode::MissingExactVersion,
                format!("{raw} does not name one exact file"),
            ));
        };
        self.selection(&routed.provider, &selection)
    }

    fn selection(&self, provider: &str, selection: &Selection) -> Result<Acquired, PackError> {
        let adapter = self.providers.adapter(provider)?;
        let http = self.client.as_deref().ok_or_else(|| {
            PackError::issue(IssueCode::HostFailure, "no network client is available")
        })?;
        let acquired = adapter.acquire(http, &selection.file, self.scratch.path())?;
        Ok(Acquired {
            path: acquired.path.clone(),
            provenance: Some(adapter.provenance(&selection.release, &acquired)),
        })
    }
}

/// Fails unless the acquired bytes are the requirement's exact digest, when it pins one.
fn verified(requirement: &PackRequirement, acquired: Acquired) -> Result<Acquired, PackError> {
    if let Some(expected) = requirement.digest {
        let found = digest_of(&acquired.path)?.ok_or_else(|| missing(expected))?;
        if found != expected {
            return Err(integrity(
                &acquired.path.display().to_string(),
                expected,
                found,
            ));
        }
    }
    Ok(acquired)
}

/// `url` with the strongest checksum the requirement pins, so the adapter verifies the download.
fn pin(url: &str, requirement: &PackRequirement) -> String {
    let base = url.split_once('#').map_or(url, |(base, _)| base);
    let identity = requirement_identity(requirement);
    match (identity.sha512, identity.sha256) {
        (Some(sha512), _) => format!("{base}#sha512={sha512}"),
        (None, Some(sha256)) => format!("{base}#sha256={sha256}"),
        (None, None) => base.to_owned(),
    }
}

fn matches_hashes(file: &ReleaseFile, requirement: &PackRequirement) -> bool {
    let equal = |published: Option<&String>, pinned: Option<&String>| matches!((published, pinned), (Some(published), Some(pinned)) if published.eq_ignore_ascii_case(pinned));
    equal(file.sha512.as_ref(), requirement.hashes.get("sha512"))
        || equal(file.sha256.as_ref(), requirement.hashes.get("sha256"))
        || equal(file.sha1.as_ref(), requirement.hashes.get("sha1"))
        || equal(file.md5.as_ref(), requirement.hashes.get("md5"))
}
