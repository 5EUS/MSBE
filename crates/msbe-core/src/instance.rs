//! Instances, profiles and deployments: the stateful layer over resolve and apply.
//!
//! An instance is one installed copy of a game. Its state lives in MSBE's data directory,
//! never inside the game directory:
//!
//! ```text
//! <home>/instances/<name>/
//!   instance.toml          where the game is, which loader, which store shard
//!   plan.toml              the plan, pinned when the instance was added
//!   journal.jsonl          the write-ahead journal for the instance root
//!   profiles/<name>.toml   the mods each profile selects
//!   deployment.json        what each live transaction deployed
//! ```
//!
//! `deployment.json` and the journal agree across crashes through a two-phase commit. The
//! intended deployment is saved as pending before its transaction runs, and the next open
//! promotes or discards it according to whether the journal shows it committed.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File},
    io::{self, BufReader},
    path::{Path, PathBuf},
};

use msbe_archive::{ArchiveError, Limits, ingest};
use msbe_fsops::{
    Applier, Backend, Capabilities, Digest, Journal, Observer, Operation, RelPath, Store, TxnId,
    atomic,
};
use msbe_plan_schema::Plan;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{ExcludedFile, ResolveError, ResolvedFile, config::Home, resolve};

/// The profile every new instance starts with.
pub const DEFAULT_PROFILE: &str = "default";

/// Device names Windows reserves regardless of extension.
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

static NOTHING_DEPLOYED: BTreeMap<RelPath, Digest> = BTreeMap::new();

/// A validated name for an instance, profile or mod.
///
/// Names become file names, so they allow only ASCII letters, digits, `.`, `_`, `-` and `+`,
/// must not start with `.`, are at most 100 characters, and must not be a device name that
/// Windows reserves.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

impl Name {
    /// Validates `raw`.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::InvalidName`] if `raw` breaks a naming rule.
    pub fn new(raw: &str) -> Result<Self, InstanceError> {
        let stem = raw.split('.').next().unwrap_or_default();
        let valid = !raw.is_empty()
            && raw.len() <= 100
            && !raw.starts_with('.')
            && raw.chars().all(is_name_char)
            && !RESERVED_NAMES
                .iter()
                .any(|reserved| reserved.eq_ignore_ascii_case(stem));
        if valid {
            Ok(Self(raw.to_owned()))
        } else {
            Err(InstanceError::InvalidName(raw.to_owned()))
        }
    }

    /// Derives a valid name from arbitrary text such as a file stem, replacing every
    /// disallowed character with `-`.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::InvalidName`] if nothing usable remains.
    pub fn sanitize(raw: &str) -> Result<Self, InstanceError> {
        let cleaned: String = raw
            .chars()
            .map(|c| if is_name_char(c) { c } else { '-' })
            .collect();
        let bounded: String = cleaned.trim_start_matches('.').chars().take(100).collect();
        Self::new(&bounded).map_err(|_| InstanceError::InvalidName(raw.to_owned()))
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

const fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+')
}

impl TryFrom<String> for Name {
    type Error = InstanceError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(&raw)
    }
}

impl From<Name> for String {
    fn from(name: Name) -> Self {
        name.0
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where an instance lives and how it is managed. Stored as `instance.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceConfig {
    /// The instance's name.
    pub name: Name,
    /// The game directory, canonicalized.
    pub root: PathBuf,
    /// The loader this instance deploys with.
    pub loader: String,
    /// The game version, used to choose compatible versions from providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub game_version: Option<String>,
    /// The store shard, on the same volume as `root` where possible.
    pub store: PathBuf,
}

/// The mods a profile selects. Stored as `profiles/<name>.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Mods by name.
    #[serde(default)]
    pub mods: BTreeMap<Name, ModEntry>,
}

/// One mod in a profile: an artifact whose files are already in the store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModEntry {
    /// The file name the mod was added from.
    pub origin: String,
    /// Where the file came from, when a provider supplied it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provenance>,
    /// Every file the artifact contained.
    pub files: Vec<StoredFile>,
}

/// A file inside an artifact, and the blob holding its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredFile {
    /// The path inside the artifact.
    pub source: RelPath,
    /// The stored contents.
    pub blob: Digest,
}

/// Where a mod came from, recorded so it can be verified, updated or fetched again.
///
/// Provider-agnostic on purpose: every provider identifies a project, a version of it, and
/// the hash it published for the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// The provider, such as `modrinth`.
    pub provider: String,
    /// The provider's stable project id.
    pub project: String,
    /// The provider's stable version id.
    pub version: String,
    /// The human-readable version number.
    pub version_number: String,
    /// The SHA-512 the provider published for the file, verified when it was downloaded.
    pub sha512: String,
}

/// A file or archive to add to a profile as one mod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// The local file or archive.
    pub path: PathBuf,
    /// The mod's name. Derived from the file stem when absent.
    pub module: Option<Name>,
    /// Where the file came from, when a provider supplied it.
    pub provider: Option<Provenance>,
}

/// Everything needed to register an instance.
#[derive(Debug, Clone, Copy)]
pub struct NewInstance<'a> {
    /// The instance's name.
    pub name: &'a Name,
    /// The game directory.
    pub root: &'a Path,
    /// The plan manifest.
    pub plan: &'a Path,
    /// The loader to deploy with, as declared by the plan.
    pub loader: &'a str,
    /// The game version, which providers need to choose compatible versions.
    pub game_version: Option<&'a str>,
    /// An explicit store location. Defaults to `.msbe/store` beside the game directory.
    pub store: Option<&'a Path>,
}

/// What one live transaction deployed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployed {
    /// The journal transaction.
    pub txn: TxnId,
    /// The profile it deployed.
    pub profile: Name,
    /// Every managed file afterwards, with its expected contents.
    pub files: BTreeMap<RelPath, Digest>,
}

/// Deployments by live transaction, plus one not yet confirmed by the journal.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeploymentState {
    #[serde(default)]
    history: Vec<Deployed>,
    #[serde(default)]
    pending: Option<Deployed>,
}

/// What deploying a profile would change. Produced without touching the instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeployPlan {
    /// The profile being deployed.
    pub profile: Name,
    /// The changes, in the order they would run.
    pub operations: Vec<Operation>,
    /// Managed files already in their target state.
    pub unchanged: usize,
    /// Files the plan withheld from deployment, with the mod and rule responsible.
    pub excluded: Vec<ModExclusion>,
    #[serde(skip)]
    target: BTreeMap<RelPath, Digest>,
}

/// A file in one mod's artifact that the plan excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModExclusion {
    /// The mod.
    pub module: Name,
    /// The excluded file and the rule responsible.
    pub file: ExcludedFile,
}

/// A path that more than one mod would place, with different contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Conflict {
    /// The destination path.
    pub path: RelPath,
    /// Every mod that claims it.
    pub claims: Vec<Claim>,
}

/// One mod's claim on a destination path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Claim {
    /// The mod.
    pub module: Name,
    /// The contents it would place.
    pub blob: Digest,
}

/// The result of a deployment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeployReport {
    /// The deployed profile.
    pub profile: Name,
    /// The committed transaction.
    pub txn: TxnId,
    /// Files placed or replaced.
    pub placed: usize,
    /// Managed files removed.
    pub removed: usize,
    /// Managed files that were already correct.
    pub unchanged: usize,
    /// How many files each materialization backend placed.
    pub backends: BTreeMap<Backend, usize>,
    /// Files the plan withheld from deployment.
    pub excluded: Vec<ModExclusion>,
}

/// Whether deployed files still match what was deployed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifyReport {
    /// The deployed profile, if anything is deployed.
    pub profile: Option<Name>,
    /// Files checked.
    pub checked: usize,
    /// Deployed files that no longer exist.
    pub missing: Vec<RelPath>,
    /// Deployed files whose contents changed.
    pub modified: Vec<RelPath>,
}

impl VerifyReport {
    /// Whether every deployed file matched.
    pub fn is_clean(&self) -> bool {
        self.missing.is_empty() && self.modified.is_empty()
    }
}

/// An overview of an instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    /// The instance's name.
    pub name: Name,
    /// The game directory.
    pub root: PathBuf,
    /// The pinned plan's id.
    pub plan_id: String,
    /// The pinned plan's version.
    pub plan_version: String,
    /// The loader.
    pub loader: String,
    /// The game version providers match, if set.
    pub game_version: Option<String>,
    /// The store shard.
    pub store: PathBuf,
    /// What the store shard can do in the game directory.
    pub capabilities: Capabilities,
    /// The backend immutable files are placed with.
    pub backend: Backend,
    /// Every profile.
    pub profiles: Vec<Name>,
    /// The profile currently deployed, if any.
    pub deployed_profile: Option<Name>,
    /// Managed files currently deployed.
    pub deployed_files: usize,
    /// Committed transactions not yet rolled back.
    pub live_transactions: usize,
    /// Interrupted transactions undone when the instance was opened.
    pub recovered: Vec<TxnId>,
}

/// An opened instance: its configuration, pinned plan, applier and deployment state.
#[derive(Debug)]
pub struct Instance {
    dir: PathBuf,
    config: InstanceConfig,
    plan: Plan,
    applier: Applier,
    state: DeploymentState,
    recovered: Vec<TxnId>,
}

impl Instance {
    /// Registers a game directory as a new instance, pins its plan, and creates an empty
    /// `default` profile.
    ///
    /// Everything that can be validated (the root, the plan, the loader, the store location)
    /// is checked before anything is written.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError`] if the instance exists, the root is not a directory, the plan
    /// is invalid or lacks the loader, or the store would sit inside the root.
    pub fn create(home: &Home, new: &NewInstance<'_>) -> Result<Self, InstanceError> {
        let dir = home.instance(new.name);
        if dir.exists() {
            return Err(InstanceError::InstanceExists(new.name.clone()));
        }
        let root =
            fs::canonicalize(new.root).map_err(io_error("resolve instance root", new.root))?;
        if !root.is_dir() {
            return Err(InstanceError::NotADirectory(root));
        }
        let plan_text = fs::read_to_string(new.plan).map_err(io_error("read plan", new.plan))?;
        let plan = parse_plan(&plan_text, new.plan)?;
        if !plan
            .loaders
            .iter()
            .any(|candidate| candidate.id == new.loader)
        {
            return Err(ResolveError::UnknownLoader(new.loader.to_owned()).into());
        }
        let store = match new.store {
            Some(path) => std::path::absolute(path).map_err(io_error("resolve store", path))?,
            None => default_store(&root)?,
        };
        if store.starts_with(&root) {
            return Err(InstanceError::StoreInsideRoot { store, root });
        }

        let profiles = dir.join("profiles");
        fs::create_dir_all(&profiles).map_err(io_error("create directory", &profiles))?;
        let config = InstanceConfig {
            name: new.name.clone(),
            root,
            loader: new.loader.to_owned(),
            game_version: new.game_version.map(str::to_owned),
            store,
        };
        write_toml(&dir.join("instance.toml"), &config)?;
        atomic::write_file(&dir.join("plan.toml"), plan_text.as_bytes())?;
        write_toml(
            &profiles.join(format!("{DEFAULT_PROFILE}.toml")),
            &Profile::default(),
        )?;
        Self::open_dir(dir)
    }

    /// Opens a registered instance, undoing any interrupted transaction and reconciling
    /// deployment state with the journal.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::UnknownInstance`], or any error loading state or recovering.
    pub fn open(home: &Home, name: &Name) -> Result<Self, InstanceError> {
        let dir = home.instance(name);
        if !dir.join("instance.toml").is_file() {
            return Err(InstanceError::UnknownInstance(name.clone()));
        }
        Self::open_dir(dir)
    }

    /// The names of every registered instance, sorted.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::Io`] if the instances directory cannot be read.
    pub fn list(home: &Home) -> Result<Vec<Name>, InstanceError> {
        let dir = home.instances();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(source) => {
                return Err(InstanceError::Io {
                    op: "list",
                    path: dir,
                    source,
                });
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(io_error("list", &dir))?;
            if let Some(raw) = entry.file_name().to_str()
                && entry.path().join("instance.toml").is_file()
                && let Ok(name) = Name::new(raw)
            {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    fn open_dir(dir: PathBuf) -> Result<Self, InstanceError> {
        let config: InstanceConfig = read_toml(&dir.join("instance.toml"))?;
        let plan_path = dir.join("plan.toml");
        let plan_text =
            fs::read_to_string(&plan_path).map_err(io_error("read plan", &plan_path))?;
        let plan = parse_plan(&plan_text, &plan_path)?;
        let store = Store::open(&config.store)?;
        let journal = Journal::open(dir.join("journal.jsonl"))?;
        let mut applier = Applier::open(&config.root, store, journal)?;
        let recovered = applier.recover()?;

        let loaded: DeploymentState = read_json_or_default(&dir.join("deployment.json"))?;
        let live = applier.journal().live_transactions();
        let mut state = loaded.clone();
        if let Some(pending) = state.pending.take()
            && live.contains(&pending.txn)
        {
            state.history.push(pending);
        }
        state
            .history
            .retain(|deployed| live.contains(&deployed.txn));

        let instance = Self {
            dir,
            config,
            plan,
            applier,
            state,
            recovered,
        };
        if instance.state != loaded {
            instance.save_state()?;
        }
        Ok(instance)
    }

    /// The instance's name.
    pub const fn name(&self) -> &Name {
        &self.config.name
    }

    /// The instance's configuration.
    pub const fn config(&self) -> &InstanceConfig {
        &self.config
    }

    /// The pinned plan.
    pub const fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Interrupted transactions undone when the instance was opened.
    pub fn recovered(&self) -> &[TxnId] {
        &self.recovered
    }

    /// The profile currently deployed, if any.
    pub fn deployed_profile(&self) -> Option<&Name> {
        self.state.history.last().map(|deployed| &deployed.profile)
    }

    /// Every managed file currently deployed, with its expected contents.
    pub fn deployed_files(&self) -> &BTreeMap<RelPath, Digest> {
        self.state
            .history
            .last()
            .map_or(&NOTHING_DEPLOYED, |deployed| &deployed.files)
    }

    /// Every profile, sorted.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::Io`] if the profiles directory cannot be read.
    pub fn profiles(&self) -> Result<Vec<Name>, InstanceError> {
        let dir = self.dir.join("profiles");
        let mut names = Vec::new();
        for entry in fs::read_dir(&dir).map_err(io_error("list", &dir))? {
            let entry = entry.map_err(io_error("list", &dir))?;
            if let Some(file) = entry.file_name().to_str()
                && let Some(stem) = file.strip_suffix(".toml")
                && let Ok(name) = Name::new(stem)
            {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// Loads a profile.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::UnknownProfile`] or a parse error.
    pub fn profile(&self, name: &Name) -> Result<Profile, InstanceError> {
        let path = self.profile_path(name);
        if !path.is_file() {
            return Err(InstanceError::UnknownProfile(name.clone()));
        }
        read_toml(&path)
    }

    /// Creates a profile, empty or copied from `from`.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::ProfileExists`], or an error loading `from` or writing.
    pub fn create_profile(
        &self,
        name: Name,
        from: Option<&Name>,
    ) -> Result<Profile, InstanceError> {
        let path = self.profile_path(&name);
        if path.exists() {
            return Err(InstanceError::ProfileExists(name));
        }
        let profile = match from {
            Some(source) => self.profile(source)?,
            None => Profile::default(),
        };
        write_toml(&path, &profile)?;
        Ok(profile)
    }

    /// Ingests files or archives into the store and adds each to `profile` as a mod. Either
    /// every artifact is added or none are.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::ModExists`] for a name already in the profile, or any ingest
    /// error.
    pub fn add_artifacts(
        &self,
        profile: &Name,
        artifacts: &[Artifact],
    ) -> Result<Vec<Name>, InstanceError> {
        self.store_artifacts(profile, artifacts, Placement::Add)
    }

    /// Replaces mods already in `profile` with new artifacts under the same names, such as
    /// newer versions from a provider. Either every mod is replaced or none are. The deployment
    /// changes only when the profile is deployed again.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::UnknownMod`] for a name the profile does not have, or any
    /// ingest error.
    pub fn replace_artifacts(
        &self,
        profile: &Name,
        artifacts: &[Artifact],
    ) -> Result<Vec<Name>, InstanceError> {
        self.store_artifacts(profile, artifacts, Placement::Replace)
    }

    fn store_artifacts(
        &self,
        profile: &Name,
        artifacts: &[Artifact],
        placement: Placement,
    ) -> Result<Vec<Name>, InstanceError> {
        let mut selection = self.profile(profile)?;
        let mut added = Vec::new();
        for artifact in artifacts {
            let module = module_name(artifact)?;
            match (placement, selection.mods.contains_key(&module)) {
                (Placement::Add, true) => {
                    return Err(InstanceError::ModExists {
                        profile: profile.clone(),
                        module,
                    });
                }
                (Placement::Replace, false) => {
                    return Err(InstanceError::UnknownMod {
                        profile: profile.clone(),
                        module,
                    });
                }
                (Placement::Add, false) | (Placement::Replace, true) => {}
            }
            let files = ingest(self.applier.store(), &artifact.path, &Limits::default())?;
            let origin = artifact
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            selection.mods.insert(
                module.clone(),
                ModEntry {
                    origin,
                    provider: artifact.provider.clone(),
                    files: files
                        .into_iter()
                        .map(|file| StoredFile {
                            source: file.source,
                            blob: file.blob,
                        })
                        .collect(),
                },
            );
            added.push(module);
        }
        write_toml(&self.profile_path(profile), &selection)?;
        Ok(added)
    }

    /// Adds local files or archives as mods named after their file stems. Either every file is
    /// added or none are.
    ///
    /// # Errors
    ///
    /// As for [`Instance::add_artifacts`].
    pub fn add_mods(&self, profile: &Name, paths: &[PathBuf]) -> Result<Vec<Name>, InstanceError> {
        let artifacts: Vec<Artifact> = paths
            .iter()
            .map(|path| Artifact {
                path: path.clone(),
                module: None,
                provider: None,
            })
            .collect();
        self.add_artifacts(profile, &artifacts)
    }

    /// Removes a mod from a profile. The deployment changes only when the profile is deployed
    /// again.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::UnknownMod`] or an error loading or writing the profile.
    pub fn remove_mod(&self, profile: &Name, module: &Name) -> Result<(), InstanceError> {
        let mut selection = self.profile(profile)?;
        if selection.mods.remove(module).is_none() {
            return Err(InstanceError::UnknownMod {
                profile: profile.clone(),
                module: module.clone(),
            });
        }
        write_toml(&self.profile_path(profile), &selection)
    }

    /// Works out what deploying `profile` would change, without touching the instance.
    ///
    /// Files already deployed with the right contents are left alone; a deployed file that
    /// went missing or changed on disk is placed again, so deploying also repairs drift.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::Conflicts`] if mods claim one path with different contents, or
    /// any resolution error.
    pub fn plan_deploy(&self, profile: &Name) -> Result<DeployPlan, InstanceError> {
        let selection = self.profile(profile)?;
        let mut claims: BTreeMap<RelPath, Vec<Claim>> = BTreeMap::new();
        let mut excluded = Vec::new();
        for (module, entry) in &selection.mods {
            let files: Vec<ResolvedFile> = entry
                .files
                .iter()
                .map(|file| ResolvedFile {
                    source: file.source.clone(),
                    blob: file.blob,
                    mutable: false,
                })
                .collect();
            let resolved = resolve(&self.plan, &self.config.loader, &files)?;
            excluded.extend(resolved.excluded.into_iter().map(|file| ModExclusion {
                module: module.clone(),
                file,
            }));
            for operation in resolved.operations {
                if let Operation::Materialize { path, blob, .. } = operation {
                    claims.entry(path).or_default().push(Claim {
                        module: module.clone(),
                        blob,
                    });
                }
            }
        }

        let mut target = BTreeMap::new();
        let mut conflicts = Vec::new();
        for (path, path_claims) in claims {
            let mut blobs: BTreeSet<Digest> = path_claims.iter().map(|claim| claim.blob).collect();
            match (blobs.pop_first(), blobs.is_empty()) {
                (Some(blob), true) => {
                    target.insert(path, blob);
                }
                _ => conflicts.push(Conflict {
                    path,
                    claims: path_claims,
                }),
            }
        }
        if !conflicts.is_empty() {
            return Err(InstanceError::Conflicts(conflicts));
        }

        let current = self.deployed_files();
        let mut operations: Vec<Operation> = current
            .keys()
            .filter(|path| !target.contains_key(*path))
            .map(|path| Operation::Remove { path: path.clone() })
            .collect();
        let mut unchanged = 0;
        for (path, blob) in &target {
            let settled = current.get(path) == Some(blob)
                && on_disk(&path.to_path(&self.config.root), blob)? == OnDisk::Matches;
            if settled {
                unchanged += 1;
            } else {
                operations.push(Operation::Materialize {
                    path: path.clone(),
                    blob: *blob,
                    mutable: false,
                });
            }
        }
        Ok(DeployPlan {
            profile: profile.clone(),
            operations,
            unchanged,
            excluded,
            target,
        })
    }

    /// Makes the instance match `profile` in one journaled transaction.
    ///
    /// # Errors
    ///
    /// Returns any planning error before anything is written. If the transaction fails it is
    /// rolled back immediately, and the instance and its state are left as they were.
    pub fn deploy(
        &mut self,
        profile: &Name,
        observer: &mut dyn Observer,
    ) -> Result<DeployReport, InstanceError> {
        let plan = self.plan_deploy(profile)?;
        let txn = self.applier.journal().next_txn();
        self.state.pending = Some(Deployed {
            txn,
            profile: profile.clone(),
            files: plan.target.clone(),
        });
        self.save_state()?;

        let committed = match self.applier.apply(&plan.operations, observer) {
            Ok(committed) => committed,
            Err(error) => {
                self.applier.recover()?;
                self.state.pending = None;
                self.save_state()?;
                return Err(error.into());
            }
        };
        if let Some(deployed) = self.state.pending.take() {
            self.state.history.push(deployed);
        }
        self.save_state()?;

        let mut backends = BTreeMap::new();
        for backend in committed.backends.values() {
            *backends.entry(*backend).or_insert(0) += 1;
        }
        let placed = plan
            .operations
            .iter()
            .filter(|operation| matches!(operation, Operation::Materialize { .. }))
            .count();
        Ok(DeployReport {
            profile: profile.clone(),
            txn: committed.txn,
            placed,
            removed: plan.operations.len() - placed,
            unchanged: plan.unchanged,
            backends,
            excluded: plan.excluded,
        })
    }

    /// Undoes the most recent deployment and returns its transaction, if there was one.
    ///
    /// # Errors
    ///
    /// Returns any error restoring files or saving state.
    pub fn rollback(&mut self) -> Result<Option<TxnId>, InstanceError> {
        let Some(txn) = self.applier.journal().live_transactions().last().copied() else {
            return Ok(None);
        };
        self.applier.rollback(txn)?;
        self.state.history.retain(|deployed| deployed.txn != txn);
        self.save_state()?;
        Ok(Some(txn))
    }

    /// Undoes every deployment, newest first, returning the instance to its state before MSBE
    /// changed it. Returns the transactions undone.
    ///
    /// # Errors
    ///
    /// Returns the first error; transactions undone before it stay undone.
    pub fn purge(&mut self) -> Result<Vec<TxnId>, InstanceError> {
        let mut undone = Vec::new();
        while let Some(txn) = self.rollback()? {
            undone.push(txn);
        }
        Ok(undone)
    }

    /// Re-hashes every deployed file against what was deployed.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::Io`] if a file exists but cannot be read.
    pub fn verify(&self) -> Result<VerifyReport, InstanceError> {
        let mut report = VerifyReport {
            profile: self.deployed_profile().cloned(),
            checked: 0,
            missing: Vec::new(),
            modified: Vec::new(),
        };
        for (path, blob) in self.deployed_files() {
            report.checked += 1;
            match on_disk(&path.to_path(&self.config.root), blob)? {
                OnDisk::Matches => {}
                OnDisk::Missing => report.missing.push(path.clone()),
                OnDisk::Differs => report.modified.push(path.clone()),
            }
        }
        Ok(report)
    }

    /// An overview of the instance.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::Io`] if the profiles cannot be listed.
    pub fn status(&self) -> Result<Status, InstanceError> {
        let capabilities = self.applier.capabilities();
        Ok(Status {
            name: self.config.name.clone(),
            root: self.config.root.clone(),
            plan_id: self.plan.id.clone(),
            plan_version: self.plan.version.clone(),
            loader: self.config.loader.clone(),
            game_version: self.config.game_version.clone(),
            store: self.config.store.clone(),
            capabilities,
            backend: capabilities.choose(&Backend::DEFAULT_CHAIN),
            profiles: self.profiles()?,
            deployed_profile: self.deployed_profile().cloned(),
            deployed_files: self.deployed_files().len(),
            live_transactions: self.applier.journal().live_transactions().len(),
            recovered: self.recovered.clone(),
        })
    }

    /// Records the game version providers should match, or clears it.
    ///
    /// # Errors
    ///
    /// Returns an error if `instance.toml` cannot be written.
    pub fn set_game_version(&mut self, version: Option<&str>) -> Result<(), InstanceError> {
        self.config.game_version = version.map(str::to_owned);
        write_toml(&self.dir.join("instance.toml"), &self.config)
    }

    /// The loader ids a provider may offer mods for: the instance's loader, then every API the
    /// plan says that loader provides.
    pub fn loader_ids(&self) -> Vec<String> {
        let mut ids = vec![self.config.loader.clone()];
        if let Some(loader) = self
            .plan
            .loaders
            .iter()
            .find(|loader| loader.id == self.config.loader)
        {
            ids.extend(loader.provides.iter().cloned());
        }
        ids
    }

    /// Scratch space on the store's volume, for downloads about to be ingested.
    pub fn scratch_dir(&self) -> PathBuf {
        self.applier.store().tmp_dir()
    }

    fn profile_path(&self, name: &Name) -> PathBuf {
        self.dir.join("profiles").join(format!("{name}.toml"))
    }

    fn save_state(&self) -> Result<(), InstanceError> {
        let path = self.dir.join("deployment.json");
        let bytes =
            serde_json::to_vec_pretty(&self.state).map_err(|error| InstanceError::Encode {
                path: path.clone(),
                reason: error.to_string(),
            })?;
        atomic::write_file(&path, &bytes)?;
        Ok(())
    }
}

/// Everything that can go wrong managing an instance.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum InstanceError {
    /// A filesystem call on MSBE's own state failed.
    #[error("{op} {}: {source}", .path.display())]
    Io {
        /// What was being attempted.
        op: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// The transactional filesystem layer failed.
    #[error(transparent)]
    Fs(#[from] msbe_fsops::Error),

    /// An artifact could not be ingested.
    #[error(transparent)]
    Archive(#[from] ArchiveError),

    /// The plan could not be resolved.
    #[error(transparent)]
    Resolve(#[from] ResolveError),

    /// A plan or state file could not be parsed.
    #[error("cannot parse {}: {reason}", .path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The parser's message.
        reason: String,
    },

    /// State could not be encoded.
    #[error("cannot encode {}: {reason}", .path.display())]
    Encode {
        /// The file.
        path: PathBuf,
        /// The encoder's message.
        reason: String,
    },

    /// A name broke the naming rules.
    #[error("invalid name {0:?}: use letters, digits, '.', '_', '-' or '+', not starting with '.'")]
    InvalidName(String),

    /// An instance with this name already exists.
    #[error("instance {0} already exists")]
    InstanceExists(Name),

    /// No instance has this name.
    #[error("no instance named {0}")]
    UnknownInstance(Name),

    /// A profile with this name already exists.
    #[error("profile {0} already exists")]
    ProfileExists(Name),

    /// No profile has this name.
    #[error("no profile named {0}")]
    UnknownProfile(Name),

    /// The profile already has a mod with this name.
    #[error("profile {profile} already has a mod named {module}")]
    ModExists {
        /// The profile.
        profile: Name,
        /// The mod.
        module: Name,
    },

    /// The profile has no mod with this name.
    #[error("profile {profile} has no mod named {module}")]
    UnknownMod {
        /// The profile.
        profile: Name,
        /// The mod.
        module: Name,
    },

    /// Mods claim the same paths with different contents.
    #[error("{} path(s) are claimed by more than one mod with different contents", .0.len())]
    Conflicts(Vec<Conflict>),

    /// The store would sit inside the game directory it serves.
    #[error("the store {} must not be inside the instance root {}", .store.display(), .root.display())]
    StoreInsideRoot {
        /// The requested store.
        store: PathBuf,
        /// The instance root.
        root: PathBuf,
    },

    /// A path that must be a directory is not one.
    #[error("{} is not a directory", .0.display())]
    NotADirectory(PathBuf),

    /// No default store location exists beside this root.
    #[error("no store location beside {}; pass one explicitly", .0.display())]
    NoDefaultStore(PathBuf),

    /// No data directory could be found.
    #[error("cannot find a data directory; set MSBE_HOME")]
    HomeUnavailable,
}

/// Parses and validates a plan manifest.
///
/// # Errors
///
/// Returns [`InstanceError::Parse`] for malformed TOML, or [`InstanceError::Resolve`] if the
/// plan breaks a schema rule.
pub fn parse_plan(text: &str, origin: &Path) -> Result<Plan, InstanceError> {
    let plan: Plan = toml::from_str(text).map_err(|error| InstanceError::Parse {
        path: origin.to_path_buf(),
        reason: error.to_string(),
    })?;
    plan.validate().map_err(ResolveError::from)?;
    Ok(plan)
}

/// Whether storing an artifact adds a new mod or replaces one of the same name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    Add,
    Replace,
}

/// The artifact's explicit name, or one derived from its file stem.
fn module_name(artifact: &Artifact) -> Result<Name, InstanceError> {
    match &artifact.module {
        Some(module) => Ok(module.clone()),
        None => Name::sanitize(
            &artifact
                .path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnDisk {
    Missing,
    Matches,
    Differs,
}

fn on_disk(path: &Path, blob: &Digest) -> Result<OnDisk, InstanceError> {
    match File::open(path) {
        Ok(file) => {
            let actual = Digest::of_reader(BufReader::new(file)).map_err(io_error("read", path))?;
            Ok(if actual == *blob {
                OnDisk::Matches
            } else {
                OnDisk::Differs
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(OnDisk::Missing),
        Err(source) => Err(InstanceError::Io {
            op: "open",
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// `.msbe/store` beside the game directory: on the same volume in the common case, and never
/// inside the directory MSBE deploys into.
fn default_store(root: &Path) -> Result<PathBuf, InstanceError> {
    root.parent()
        .map(|parent| parent.join(".msbe").join("store"))
        .ok_or_else(|| InstanceError::NoDefaultStore(root.to_path_buf()))
}

fn io_error<'a>(op: &'static str, path: &'a Path) -> impl FnOnce(io::Error) -> InstanceError + 'a {
    move |source| InstanceError::Io {
        op,
        path: path.to_path_buf(),
        source,
    }
}

fn read_toml<T: DeserializeOwned>(path: &Path) -> Result<T, InstanceError> {
    let text = fs::read_to_string(path).map_err(io_error("read", path))?;
    toml::from_str(&text).map_err(|error| InstanceError::Parse {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })
}

fn write_toml<T: Serialize>(path: &Path, value: &T) -> Result<(), InstanceError> {
    let text = toml::to_string_pretty(value).map_err(|error| InstanceError::Encode {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    atomic::write_file(path, text.as_bytes())?;
    Ok(())
}

fn read_json_or_default<T: DeserializeOwned + Default>(path: &Path) -> Result<T, InstanceError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| InstanceError::Parse {
            path: path.to_path_buf(),
            reason: error.to_string(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(source) => Err(InstanceError::Io {
            op: "read",
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::PathBuf};

    use msbe_fsops::{Checkpoint, Error as FsError, NoopObserver, Observer, RelPath};
    use tempfile::TempDir;

    use super::{
        Deployed, DeploymentState, Instance, InstanceError, ModEntry, Name, Profile, StoredFile,
        write_toml,
    };
    use crate::config::Home;

    const PLAN: &str = r#"
schema = 1
id = "example"
name = "Example"
version = "1.0.0"

[[loaders]]
id = "loader"
provides = ["base-api"]
bootstrap = "none"
targets = [{ name = "mods", path = "mods" }]

[[steps]]
type = "place"

[steps.with]
into = "@loader.targets.mods"
flatten = true
"#;

    struct Fixture {
        _dir: TempDir,
        home: Home,
        game: PathBuf,
        plan: PathBuf,
        inputs: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let game = dir.path().join("game");
            fs::create_dir_all(game.join("mods")).unwrap();
            fs::write(game.join("mods/existing.bin"), b"vanilla").unwrap();
            let plan = dir.path().join("plan.toml");
            fs::write(&plan, PLAN).unwrap();
            let inputs = dir.path().join("inputs");
            fs::create_dir_all(&inputs).unwrap();
            Self {
                home: Home::at(dir.path().join("home")),
                game,
                plan,
                inputs,
                _dir: dir,
            }
        }

        fn create(&self) -> Instance {
            Instance::create(
                &self.home,
                &super::NewInstance {
                    name: &name("demo"),
                    root: &self.game,
                    plan: &self.plan,
                    loader: "loader",
                    game_version: Some("1.0"),
                    store: None,
                },
            )
            .unwrap()
        }

        fn reopen(&self) -> Instance {
            Instance::open(&self.home, &name("demo")).unwrap()
        }

        fn input(&self, file: &str, bytes: &[u8]) -> PathBuf {
            let path = self.inputs.join(file);
            fs::write(&path, bytes).unwrap();
            path
        }

        fn state_path(&self) -> PathBuf {
            self.home.instance(&name("demo")).join("deployment.json")
        }

        fn state(&self) -> DeploymentState {
            fs::read(self.state_path()).map_or_else(
                |_| DeploymentState::default(),
                |bytes| serde_json::from_slice(&bytes).unwrap(),
            )
        }

        fn write_state(&self, state: &DeploymentState) {
            fs::write(self.state_path(), serde_json::to_vec(state).unwrap()).unwrap();
        }
    }

    fn name(raw: &str) -> Name {
        Name::new(raw).unwrap()
    }

    struct AbortBeforeCommit;

    impl Observer for AbortBeforeCommit {
        fn checkpoint(&mut self, at: Checkpoint) -> msbe_fsops::Result<()> {
            if at == Checkpoint::BeforeCommit {
                Err(FsError::Aborted(at))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn names_are_safe_file_names_on_every_platform() {
        for good in ["default", "sodium-0.6.0", "a_b+c"] {
            assert!(Name::new(good).is_ok(), "{good}");
        }
        for bad in ["", ".hidden", "a/b", "a b", "con", "NUL", "lpt3.toml"] {
            assert!(Name::new(bad).is_err(), "{bad:?}");
        }
        assert!(Name::new(&"x".repeat(101)).is_err());
        assert_eq!(
            Name::sanitize("Iris Shaders (1.8)").unwrap().as_str(),
            "Iris-Shaders--1.8-"
        );
    }

    #[test]
    fn deploy_then_purge_restores_the_game_directory_exactly() {
        let fixture = Fixture::new();
        let mut instance = fixture.create();
        let default = name("default");
        instance
            .add_mods(
                &default,
                &[
                    fixture.input("alpha.bin", b"alpha"),
                    fixture.input("existing.bin", b"replacement"),
                ],
            )
            .unwrap();

        let report = instance.deploy(&default, &mut NoopObserver).unwrap();
        assert_eq!((report.placed, report.removed), (2, 0));
        assert_eq!(
            fs::read(fixture.game.join("mods/existing.bin")).unwrap(),
            b"replacement"
        );

        assert_eq!(instance.purge().unwrap(), [report.txn]);
        assert_eq!(
            fs::read(fixture.game.join("mods/existing.bin")).unwrap(),
            b"vanilla"
        );
        assert!(!fixture.game.join("mods/alpha.bin").exists());
        assert!(instance.deployed_profile().is_none());
    }

    #[test]
    fn a_crash_after_commit_is_promoted_on_the_next_open() {
        let fixture = Fixture::new();
        let mut instance = fixture.create();
        let default = name("default");
        instance
            .add_mods(&default, &[fixture.input("alpha.bin", b"alpha")])
            .unwrap();
        instance.deploy(&default, &mut NoopObserver).unwrap();
        drop(instance);

        // Rewind deployment.json to how it looked between the commit and the promotion.
        let mut state = fixture.state();
        state.pending = state.history.pop();
        fixture.write_state(&state);

        let reopened = fixture.reopen();
        assert_eq!(reopened.deployed_profile(), Some(&default));
        assert_eq!(reopened.deployed_files().len(), 1);
        assert!(fixture.state().pending.is_none());
    }

    #[test]
    fn a_pending_deployment_that_never_committed_is_discarded() {
        let fixture = Fixture::new();
        drop(fixture.create());
        fixture.write_state(&DeploymentState {
            history: Vec::new(),
            pending: Some(Deployed {
                txn: serde_json::from_str("99").unwrap(),
                profile: name("default"),
                files: BTreeMap::new(),
            }),
        });

        let reopened = fixture.reopen();
        assert!(reopened.deployed_profile().is_none());
        assert_eq!(fixture.state(), DeploymentState::default());
    }

    #[test]
    fn a_rollback_interrupted_before_saving_state_is_reconciled() {
        let fixture = Fixture::new();
        let mut instance = fixture.create();
        let default = name("default");
        instance
            .add_mods(&default, &[fixture.input("alpha.bin", b"alpha")])
            .unwrap();
        let report = instance.deploy(&default, &mut NoopObserver).unwrap();
        // Undo the transaction in the journal only, as if the process died before saving.
        instance.applier.rollback(report.txn).unwrap();
        drop(instance);

        let reopened = fixture.reopen();
        assert!(reopened.deployed_profile().is_none());
        assert!(!fixture.game.join("mods/alpha.bin").exists());
    }

    #[test]
    fn an_aborted_deploy_leaves_the_instance_and_its_state_untouched() {
        let fixture = Fixture::new();
        let mut instance = fixture.create();
        let default = name("default");
        instance
            .add_mods(&default, &[fixture.input("alpha.bin", b"alpha")])
            .unwrap();

        let error = instance
            .deploy(&default, &mut AbortBeforeCommit)
            .unwrap_err();
        assert!(
            matches!(
                error,
                InstanceError::Fs(FsError::Aborted(Checkpoint::BeforeCommit))
            ),
            "{error:?}"
        );
        assert!(!fixture.game.join("mods/alpha.bin").exists());
        assert!(instance.deployed_profile().is_none());
        assert_eq!(fixture.state(), DeploymentState::default());
    }

    #[test]
    fn provenance_game_version_and_loader_apis_survive_reopening() {
        let fixture = Fixture::new();
        let mut instance = fixture.create();
        let default = name("default");
        let provenance = super::Provenance {
            provider: "example".to_owned(),
            project: "P1".to_owned(),
            version: "V1".to_owned(),
            version_number: "1.0.0".to_owned(),
            sha512: "ab".repeat(64),
        };
        instance
            .add_artifacts(
                &default,
                &[super::Artifact {
                    path: fixture.input("download.bin", b"remote"),
                    module: Some(name("remote-mod")),
                    provider: Some(provenance.clone()),
                }],
            )
            .unwrap();
        instance.set_game_version(Some("2.0")).unwrap();
        drop(instance);

        let reopened = fixture.reopen();
        assert_eq!(reopened.config().game_version.as_deref(), Some("2.0"));
        assert_eq!(reopened.loader_ids(), ["loader", "base-api"]);
        let profile = reopened.profile(&default).unwrap();
        assert_eq!(
            profile
                .mods
                .get(&name("remote-mod"))
                .and_then(|entry| entry.provider.as_ref()),
            Some(&provenance)
        );
    }

    #[test]
    fn replacing_an_artifact_keeps_its_name_and_swaps_its_files_and_provenance() {
        let fixture = Fixture::new();
        let mut instance = fixture.create();
        let default = name("default");
        let artifact = |file: &str, bytes: &[u8], version: &str| super::Artifact {
            path: fixture.input(file, bytes),
            module: Some(name("remote-mod")),
            provider: Some(super::Provenance {
                provider: "example".to_owned(),
                project: "P1".to_owned(),
                version: version.to_owned(),
                version_number: version.to_owned(),
                sha512: "ab".repeat(64),
            }),
        };
        instance
            .add_artifacts(&default, &[artifact("remote-1.0.bin", b"one", "1.0")])
            .unwrap();
        instance.deploy(&default, &mut NoopObserver).unwrap();

        instance
            .replace_artifacts(&default, &[artifact("remote-2.0.bin", b"two", "2.0")])
            .unwrap();
        let entry = instance
            .profile(&default)
            .unwrap()
            .mods
            .remove(&name("remote-mod"))
            .unwrap();
        assert_eq!(entry.origin, "remote-2.0.bin");
        assert_eq!(
            entry.provider.map(|provider| provider.version),
            Some("2.0".to_owned())
        );

        let report = instance.deploy(&default, &mut NoopObserver).unwrap();
        assert_eq!((report.placed, report.removed), (1, 1));
        assert_eq!(
            fs::read(fixture.game.join("mods/remote-2.0.bin")).unwrap(),
            b"two"
        );
        assert!(!fixture.game.join("mods/remote-1.0.bin").exists());

        let absent = super::Artifact {
            module: Some(name("absent")),
            ..artifact("absent.bin", b"x", "1.0")
        };
        let result = instance.replace_artifacts(&default, &[absent]);
        assert!(
            matches!(result, Err(InstanceError::UnknownMod { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn adding_artifacts_is_all_or_nothing() {
        let fixture = Fixture::new();
        let instance = fixture.create();
        let default = name("default");
        let result = instance.add_artifacts(
            &default,
            &[
                super::Artifact {
                    path: fixture.input("good.bin", b"good"),
                    module: None,
                    provider: None,
                },
                super::Artifact {
                    path: fixture.inputs.join("does-not-exist.bin"),
                    module: None,
                    provider: None,
                },
            ],
        );
        assert!(result.is_err());
        assert!(instance.profile(&default).unwrap().mods.is_empty());
    }

    #[test]
    fn mods_claiming_one_path_with_different_contents_conflict() {
        let fixture = Fixture::new();
        let instance = fixture.create();
        let default = name("default");
        let store = instance.applier.store();
        let first = store.put_bytes(b"version a").unwrap();
        let second = store.put_bytes(b"version b").unwrap();
        let entry = |source: &str, blob| ModEntry {
            origin: format!("{source}.zip"),
            provider: None,
            files: vec![StoredFile {
                source: RelPath::new(source).unwrap(),
                blob,
            }],
        };
        let profile = Profile {
            mods: BTreeMap::from([
                (name("first"), entry("a/common.bin", first)),
                (name("second"), entry("b/common.bin", second)),
            ]),
        };
        write_toml(&instance.profile_path(&default), &profile).unwrap();

        let conflicts = match instance.plan_deploy(&default) {
            Err(InstanceError::Conflicts(conflicts)) => conflicts,
            other => panic!("expected conflicts, got {other:?}"),
        };
        let [conflict] = conflicts.as_slice() else {
            panic!("expected one conflict, got {conflicts:?}");
        };
        assert_eq!(conflict.path.as_str(), "mods/common.bin");
        let modules: Vec<&str> = conflict
            .claims
            .iter()
            .map(|claim| claim.module.as_str())
            .collect();
        assert_eq!(modules, ["first", "second"]);
    }
}
