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
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File},
    io::{self, BufReader},
    path::{Path, PathBuf},
};

use msbe_archive::{ArchiveError, Limits, ingest, ingest_as_file};
use msbe_fsops::{
    Applier, Backend, Capabilities, Digest, Journal, Observer, Operation, RelPath, Store, TxnId,
    atomic,
};
use msbe_plan_schema::{Component, Plan, Side};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::bisect::{Error as BisectError, Session as BisectSession};
use crate::{ExcludedFile, ResolveError, ResolvedFile, config::Home, resolve};

/// The profile every new instance starts with.
pub const DEFAULT_PROFILE: &str = "default";

/// Device names Windows reserves regardless of extension.
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

static NOTHING_DEPLOYED: BTreeMap<RelPath, Digest> = BTreeMap::new();

type BootstrapClaims = Vec<(RelPath, Claim)>;

const fn default_side() -> Side {
    Side::Client
}

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
    /// The selected loader version, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
    /// Whether profiles target a player client or dedicated server.
    #[serde(default = "default_side")]
    pub side: Side,
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
    /// The compatibility target for this profile. Profiles written before M2 inherit the
    /// instance target until explicitly configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ProfileTarget>,
    /// Verified bootstrap component bundles, keyed by component ID.
    #[serde(default)]
    pub components: BTreeMap<String, ComponentEntry>,
    /// Mods by name.
    #[serde(default)]
    pub mods: BTreeMap<Name, ModEntry>,
}

/// A verified component bundle retained in the content store for one profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentEntry {
    /// Exact reviewed component version.
    pub version: String,
    /// SHA-512 verified by the acquiring component adapter.
    pub sha512: String,
    /// Files extracted from the verified bundle.
    pub files: Vec<StoredFile>,
}

/// The loader-specific part of a profile compatibility target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileTarget {
    /// The loader selected by this profile.
    pub loader: String,
    /// The selected loader version, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
    /// Whether this profile targets a player client or dedicated server.
    pub side: Side,
}

/// A portable, reproducible snapshot of one resolved profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lockfile {
    /// Lockfile schema version.
    pub schema: u32,
    /// The pinned plan identity.
    pub plan: LockedPlan,
    /// The compatibility target used to select artifacts.
    pub target: LockedTarget,
    /// Resolved modules keyed by stable local module name.
    pub mods: BTreeMap<Name, LockedModule>,
    /// Pinned loader component bundles.
    #[serde(default)]
    pub components: BTreeMap<String, ComponentEntry>,
    /// Exact portable deployment shape, independent of filesystem backend.
    pub deployment: BTreeMap<RelPath, Digest>,
}

/// The exact plan used to resolve a lockfile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPlan {
    /// Stable plan identifier.
    pub id: String,
    /// Exact plan version.
    pub version: String,
}

/// Instance facts needed to reproduce provider compatibility filtering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedTarget {
    /// Game version used for candidate filtering.
    pub game_version: Option<String>,
    /// Selected loader identifier.
    pub loader: String,
    /// Selected loader version, when known.
    pub loader_version: Option<String>,
    /// Player-client or dedicated-server target.
    pub side: Side,
}

/// One resolved module and its exact content-addressed files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedModule {
    /// Original local artifact name.
    pub origin: String,
    /// Provider release identity and published digest, when available.
    pub provider: Option<Provenance>,
    /// Exact CAS files included by this module.
    pub files: Vec<StoredFile>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Provenance {
    /// The provider, such as `modrinth`.
    pub provider: String,
    /// The provider's stable project id.
    pub project: String,
    /// The provider's stable version id.
    pub version: String,
    /// The human-readable version number.
    pub version_number: String,
    /// Provider-published artifact hashes, keyed by normalized algorithm such as `sha256`.
    pub hashes: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for Provenance {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            provider: String,
            project: String,
            version: String,
            version_number: String,
            #[serde(default)]
            hashes: BTreeMap<String, String>,
            #[serde(default)]
            sha512: Option<String>,
        }

        let wire = Wire::deserialize(deserializer)?;
        let mut hashes = wire.hashes;
        if let Some(sha512) = wire.sha512 {
            hashes.entry("sha512".to_owned()).or_insert(sha512);
        }
        Ok(Self {
            provider: wire.provider,
            project: wire.project,
            version: wire.version,
            version_number: wire.version_number,
            hashes,
        })
    }
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
    /// Verified logical source path that a deployment plan may use for routing.
    pub source: Option<RelPath>,
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
    /// The selected loader version, when known.
    pub loader_version: Option<&'a str>,
    /// Whether profiles target a player client or dedicated server.
    pub side: Side,
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
    /// The managed files the plan declares mutable. Runtime changes to them are expected.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub mutable: BTreeSet<RelPath>,
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
    /// Mutable files changed on disk whose mod now ships a different default. The local
    /// changes are kept and the new default is not applied.
    pub kept: Vec<RelPath>,
    /// Files the plan withheld from deployment, with the mod and rule responsible.
    pub excluded: Vec<ModExclusion>,
    #[serde(skip)]
    target: BTreeMap<RelPath, Digest>,
    #[serde(skip)]
    mutable: BTreeSet<RelPath>,
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
    /// Empty directories MSBE had created that were removed.
    pub removed_dirs: usize,
    /// Managed files that were already correct.
    pub unchanged: usize,
    /// Mutable files whose local changes were kept instead of a new default.
    pub kept: Vec<RelPath>,
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
    /// Mutable files the game or a mod changed, as expected. These are not drift.
    pub changed_at_runtime: Vec<RelPath>,
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
        let loader = plan
            .loaders
            .iter()
            .find(|candidate| candidate.id == new.loader)
            .ok_or_else(|| ResolveError::UnknownLoader(new.loader.to_owned()))?;
        if !loader.sides.is_empty() && !loader.sides.contains(&new.side) {
            return Err(InstanceError::UnsupportedSide {
                loader: new.loader.to_owned(),
                side: new.side,
            });
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
            loader_version: new.loader_version.map(str::to_owned),
            side: new.side,
            game_version: new.game_version.map(str::to_owned),
            store,
        };
        write_toml(&dir.join("instance.toml"), &config)?;
        atomic::write_file(&dir.join("plan.toml"), plan_text.as_bytes())?;
        write_toml(
            &profiles.join(format!("{DEFAULT_PROFILE}.toml")),
            &Profile {
                target: Some(ProfileTarget {
                    loader: config.loader.clone(),
                    loader_version: config.loader_version.clone(),
                    side: config.side,
                }),
                ..Profile::default()
            },
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

    /// Derives the canonical lockfile for `profile` without writing it.
    ///
    /// # Errors
    ///
    /// Returns an error when the profile is missing or its resolved deployment is invalid.
    pub fn lockfile(&self, profile: &Name) -> Result<Lockfile, InstanceError> {
        let selection = self.profile(profile)?;
        let target = self.profile_target(profile)?;
        let deployment = self.plan_deploy(profile)?.target;
        Ok(Lockfile {
            schema: 1,
            plan: LockedPlan {
                id: self.plan.id.clone(),
                version: self.plan.version.clone(),
            },
            target: LockedTarget {
                game_version: self.config.game_version.clone(),
                loader: target.loader,
                loader_version: target.loader_version,
                side: target.side,
            },
            mods: selection
                .mods
                .into_iter()
                .map(|(name, module)| {
                    (
                        name,
                        LockedModule {
                            origin: module.origin,
                            provider: module.provider,
                            files: module.files,
                        },
                    )
                })
                .collect(),
            components: selection.components,
            deployment,
        })
    }

    /// Writes a canonical lockfile in the instance's portable lockfile directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the lockfile cannot be derived or written.
    pub fn write_lockfile(&self, profile: &Name) -> Result<Lockfile, InstanceError> {
        let lockfile = self.lockfile(profile)?;
        let directory = self.dir.join("locks");
        fs::create_dir_all(&directory).map_err(io_error("create directory", &directory))?;
        write_toml(&directory.join(format!("{profile}.toml")), &lockfile)?;
        Ok(lockfile)
    }

    /// Starts and persists a bisection for every module in `profile`.
    ///
    /// # Errors
    ///
    /// Returns an error when the profile cannot be loaded, has fewer than two modules, or state
    /// cannot be written.
    pub fn start_bisect(&self, profile: &Name) -> Result<BisectSession, InstanceError> {
        let source = self.profile(profile)?;
        let trial_profile = Name::new("msbe-bisect")?;
        if self.profile_path(&trial_profile).exists() {
            return Err(InstanceError::ProfileExists(trial_profile));
        }
        let candidates = source.mods.keys().cloned().collect();
        let session = BisectSession::new(profile.clone(), trial_profile, candidates)?;
        self.write_bisect_trial(&source, &session)?;
        write_toml(&self.dir.join("bisect.toml"), &session)?;
        Ok(session)
    }

    /// Loads the resumable bisection session.
    ///
    /// # Errors
    ///
    /// Returns an error when no bisection is active or its state is invalid.
    pub fn bisect(&self) -> Result<BisectSession, InstanceError> {
        let path = self.dir.join("bisect.toml");
        if !path.is_file() {
            return Err(InstanceError::NoBisect);
        }
        read_toml(&path)
    }

    /// Records a bisection verdict and persists the next deterministic trial.
    ///
    /// # Errors
    ///
    /// Returns an error when no bisection is active or its state cannot be written.
    pub fn record_bisect(&self, failing: bool) -> Result<BisectSession, InstanceError> {
        let mut session = self.bisect()?;
        session.record(failing)?;
        if !session.trial.is_empty() {
            self.write_bisect_trial(&self.profile(&session.profile)?, &session)?;
        }
        write_toml(&self.dir.join("bisect.toml"), &session)?;
        Ok(session)
    }

    /// Deploys the current bisection trial profile.
    ///
    /// # Errors
    ///
    /// Returns an error when no bisection is active or the deployment fails.
    pub fn run_bisect(
        &mut self,
        observer: &mut dyn Observer,
    ) -> Result<DeployReport, InstanceError> {
        let session = self.bisect()?;
        self.deploy(&session.trial_profile, observer)
    }

    /// Restores the source profile and removes all bisection state.
    ///
    /// # Errors
    ///
    /// Returns an error when no bisection is active or cleanup cannot be completed.
    pub fn finish_bisect(
        &mut self,
        observer: &mut dyn Observer,
    ) -> Result<DeployReport, InstanceError> {
        let session = self.bisect()?;
        let report = self.deploy(&session.profile, observer)?;
        #[expect(
            clippy::disallowed_methods,
            reason = "the restored profile is deployed before deleting the MSBE-owned trial metadata"
        )]
        fs::remove_file(self.profile_path(&session.trial_profile)).map_err(io_error(
            "remove",
            &self.profile_path(&session.trial_profile),
        ))?;
        #[expect(
            clippy::disallowed_methods,
            reason = "the completed session is MSBE-owned metadata outside the game root"
        )]
        fs::remove_file(self.dir.join("bisect.toml"))
            .map_err(io_error("remove", &self.dir.join("bisect.toml")))?;
        Ok(report)
    }

    fn write_bisect_trial(
        &self,
        source: &Profile,
        session: &BisectSession,
    ) -> Result<(), InstanceError> {
        let mut trial = source.clone();
        trial.mods.retain(|name, _| session.trial.contains(name));
        write_toml(&self.profile_path(&session.trial_profile), &trial)
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
            None => Profile {
                target: Some(self.default_target()),
                ..Profile::default()
            },
        };
        write_toml(&path, &profile)?;
        Ok(profile)
    }

    /// Deletes an inactive profile and its derived lockfile.
    ///
    /// # Errors
    ///
    /// Returns an error when the profile is active, missing, or its state files cannot be
    /// removed.
    pub fn remove_profile(&self, profile: &Name) -> Result<(), InstanceError> {
        if self.deployed_profile() == Some(profile) {
            return Err(InstanceError::ProfileDeployed(profile.clone()));
        }
        let path = self.profile_path(profile);
        if !path.is_file() {
            return Err(InstanceError::UnknownProfile(profile.clone()));
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "an inactive profile is MSBE-owned metadata outside the game root"
        )]
        fs::remove_file(&path).map_err(io_error("remove", &path))?;
        let lockfile = self.dir.join("locks").join(format!("{profile}.toml"));
        #[expect(
            clippy::disallowed_methods,
            reason = "a derived lockfile is MSBE-owned metadata outside the game root"
        )]
        if let Err(error) = fs::remove_file(&lockfile)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(InstanceError::Io {
                op: "remove",
                path: lockfile,
                source: error,
            });
        }
        Ok(())
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

    /// Records a verified bootstrap bundle for the profile's selected loader.
    ///
    /// The bundle must exactly match a component declared by the plan. Its files are later
    /// materialized through the normal deployment transaction, never executed by MSBE.
    ///
    /// # Errors
    ///
    /// Returns an error when the component is not the selected loader's bootstrap, is not the
    /// reviewed version or digest, or omits a declared bundle file.
    pub fn install_component(
        &self,
        profile: &Name,
        component_id: &str,
        entry: ComponentEntry,
    ) -> Result<(), InstanceError> {
        let target = self.profile_target(profile)?;
        let loader = self
            .plan
            .loaders
            .iter()
            .find(|loader| loader.id == target.loader)
            .ok_or_else(|| ResolveError::UnknownLoader(target.loader.clone()))?;
        if loader.bootstrap != component_id {
            return Err(InstanceError::UnexpectedComponent {
                loader: loader.id.clone(),
                component: component_id.to_owned(),
            });
        }
        let component = self.component(component_id)?;
        if component.version != entry.version
            || !component.sha512.eq_ignore_ascii_case(&entry.sha512)
        {
            return Err(InstanceError::ComponentMismatch(component_id.to_owned()));
        }
        for file in &component.files {
            if !entry
                .files
                .iter()
                .any(|stored| stored.source.as_str() == file.source)
            {
                return Err(InstanceError::MissingComponentFile {
                    component: component_id.to_owned(),
                    path: file.source.clone(),
                });
            }
        }
        let mut selection = self.profile(profile)?;
        selection.components.insert(component_id.to_owned(), entry);
        write_toml(&self.profile_path(profile), &selection)
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
            let files = match artifact.source.clone() {
                Some(source) => ingest_as_file(
                    self.applier.store(),
                    &artifact.path,
                    source,
                    &Limits::default(),
                )?,
                None => ingest(self.applier.store(), &artifact.path, &Limits::default())?,
            };
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
                source: None,
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

    /// Returns `profile`'s target, inheriting the legacy instance target when absent.
    ///
    /// # Errors
    ///
    /// Returns an error if the profile is missing or its target loader is invalid for the plan.
    pub fn profile_target(&self, profile: &Name) -> Result<ProfileTarget, InstanceError> {
        let target = self
            .profile(profile)?
            .target
            .unwrap_or_else(|| self.default_target());
        self.validate_target(&target)?;
        Ok(target)
    }

    fn component(&self, id: &str) -> Result<&Component, InstanceError> {
        self.plan
            .components
            .iter()
            .find(|component| component.id == id)
            .ok_or_else(|| InstanceError::UnknownComponent(id.to_owned()))
    }

    fn bootstrap_claims(
        &self,
        loader_id: &str,
        entries: &BTreeMap<String, ComponentEntry>,
    ) -> Result<BootstrapClaims, InstanceError> {
        let loader = self
            .plan
            .loaders
            .iter()
            .find(|loader| loader.id == loader_id)
            .ok_or_else(|| ResolveError::UnknownLoader(loader_id.to_owned()))?;
        if self.plan.components.is_empty() || loader.bootstrap == "none" {
            return Ok(Vec::new());
        }
        let entry = entries
            .get(&loader.bootstrap)
            .ok_or_else(|| InstanceError::MissingComponent(loader.bootstrap.clone()))?;
        let component = self.component(&loader.bootstrap)?;
        component
            .files
            .iter()
            .map(|file| {
                let stored = entry
                    .files
                    .iter()
                    .find(|stored| stored.source.as_str() == file.source)
                    .ok_or_else(|| InstanceError::MissingComponentFile {
                        component: component.id.clone(),
                        path: file.source.clone(),
                    })?;
                Ok((
                    RelPath::new(&file.path)?,
                    Claim {
                        module: Name::new(&format!("component-{}", component.id))?,
                        blob: stored.blob,
                    },
                ))
            })
            .collect()
    }

    /// Sets a profile-specific compatibility target.
    ///
    /// # Errors
    ///
    /// Returns an error if the profile is missing, its target is invalid for the plan, or it
    /// cannot be written.
    pub fn set_profile_target(
        &self,
        profile: &Name,
        target: ProfileTarget,
    ) -> Result<(), InstanceError> {
        self.validate_target(&target)?;
        let mut selection = self.profile(profile)?;
        selection.target = Some(target);
        write_toml(&self.profile_path(profile), &selection)
    }

    /// Works out what deploying `profile` would change, without touching the instance.
    ///
    /// Files already deployed with the right contents are left alone; a deployed file that
    /// went missing or changed on disk is placed again, so deploying also repairs drift.
    /// Mutable files are the exception: runtime changes to them are kept (see
    /// [`DeployPlan::kept`]). Empty directories MSBE created that nothing needs any more are
    /// removed.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceError::Conflicts`] if mods claim one path with different contents, or
    /// any resolution error.
    pub fn plan_deploy(&self, profile: &Name) -> Result<DeployPlan, InstanceError> {
        let selection = self.profile(profile)?;
        let target = selection.target.unwrap_or_else(|| self.default_target());
        self.validate_target(&target)?;
        let mut claims: BTreeMap<RelPath, Vec<Claim>> = BTreeMap::new();
        let mut mutable = BTreeSet::new();
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
            let resolved = resolve(&self.plan, &target.loader, &files)?;
            excluded.extend(resolved.excluded.into_iter().map(|file| ModExclusion {
                module: module.clone(),
                file,
            }));
            for operation in resolved.operations {
                if let Operation::Materialize {
                    path,
                    blob,
                    mutable: declared,
                } = operation
                {
                    if declared {
                        mutable.insert(path.clone());
                    }
                    claims.entry(path).or_default().push(Claim {
                        module: module.clone(),
                        blob,
                    });
                }
            }
        }
        for (path, claim) in self.bootstrap_claims(&target.loader, &selection.components)? {
            claims.entry(path).or_default().push(claim);
        }
        let target = settle_claims(claims)?;

        let current = self.deployed_files();
        let removing: BTreeSet<RelPath> = current
            .keys()
            .filter(|path| !target.contains_key(*path))
            .cloned()
            .collect();
        let mut operations: Vec<Operation> = removing
            .iter()
            .map(|path| Operation::Remove { path: path.clone() })
            .collect();
        operations.extend(
            self.prunable_dirs(&target, &removing)?
                .into_iter()
                .map(|path| Operation::RemoveDir { path }),
        );
        let mut unchanged = 0;
        let mut kept = Vec::new();
        for (path, blob) in &target {
            let is_mutable = mutable.contains(path);
            match self.settle(path, blob, current.get(path), is_mutable)? {
                Settle::Unchanged => unchanged += 1,
                Settle::Keep => kept.push(path.clone()),
                Settle::Place => operations.push(Operation::Materialize {
                    path: path.clone(),
                    blob: *blob,
                    mutable: is_mutable,
                }),
            }
        }
        Ok(DeployPlan {
            profile: profile.clone(),
            operations,
            unchanged,
            kept,
            excluded,
            target,
            mutable,
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
            mutable: plan.mutable.clone(),
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
        let count = |kind: fn(&Operation) -> bool| {
            plan.operations
                .iter()
                .filter(|operation| kind(operation))
                .count()
        };
        Ok(DeployReport {
            profile: profile.clone(),
            txn: committed.txn,
            placed: count(|operation| matches!(operation, Operation::Materialize { .. })),
            removed: count(|operation| matches!(operation, Operation::Remove { .. })),
            removed_dirs: count(|operation| matches!(operation, Operation::RemoveDir { .. })),
            unchanged: plan.unchanged,
            kept: plan.kept,
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
            changed_at_runtime: Vec::new(),
        };
        let mutable = self.state.history.last().map(|deployed| &deployed.mutable);
        for (path, blob) in self.deployed_files() {
            report.checked += 1;
            match on_disk(&path.to_path(&self.config.root), blob)? {
                OnDisk::Matches => {}
                OnDisk::Missing => report.missing.push(path.clone()),
                OnDisk::Differs if mutable.is_some_and(|mutable| mutable.contains(path)) => {
                    report.changed_at_runtime.push(path.clone());
                }
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

    /// Records the target facts providers use for compatibility filtering.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected side is not supported by the configured loader or the
    /// updated configuration cannot be written.
    pub fn set_target(
        &mut self,
        loader_version: Option<&str>,
        side: Side,
    ) -> Result<(), InstanceError> {
        self.validate_target(&ProfileTarget {
            loader: self.config.loader.clone(),
            loader_version: loader_version.map(str::to_owned),
            side,
        })?;
        self.config.loader_version = loader_version.map(str::to_owned);
        self.config.side = side;
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

    /// The virtual loader APIs provided by the selected loader.
    pub fn loader_provides(&self) -> Vec<String> {
        self.plan
            .loaders
            .iter()
            .find(|loader| loader.id == self.config.loader)
            .map_or_else(Vec::new, |loader| loader.provides.clone())
    }

    /// Returns the virtual loader APIs available to `target`.
    pub fn target_provides(&self, target: &ProfileTarget) -> Vec<String> {
        self.plan
            .loaders
            .iter()
            .find(|loader| loader.id == target.loader)
            .map_or_else(Vec::new, |loader| loader.provides.clone())
    }

    fn default_target(&self) -> ProfileTarget {
        ProfileTarget {
            loader: self.config.loader.clone(),
            loader_version: self.config.loader_version.clone(),
            side: self.config.side,
        }
    }

    fn validate_target(&self, target: &ProfileTarget) -> Result<(), InstanceError> {
        let loader = self
            .plan
            .loaders
            .iter()
            .find(|loader| loader.id == target.loader)
            .ok_or_else(|| ResolveError::UnknownLoader(target.loader.clone()))?;
        if !loader.sides.is_empty() && !loader.sides.contains(&target.side) {
            return Err(InstanceError::UnsupportedSide {
                loader: loader.id.clone(),
                side: target.side,
            });
        }
        Ok(())
    }

    /// Scratch space on the store's volume, for downloads about to be ingested.
    pub fn scratch_dir(&self) -> PathBuf {
        self.applier.store().tmp_dir()
    }

    /// What deploying `blob` at `path` needs, given the blob deployed there before, if any.
    fn settle(
        &self,
        path: &RelPath,
        blob: &Digest,
        deployed: Option<&Digest>,
        mutable: bool,
    ) -> Result<Settle, InstanceError> {
        let file = path.to_path(&self.config.root);
        let disk = on_disk(&file, blob)?;
        if !mutable {
            let settled = deployed == Some(blob) && disk == OnDisk::Matches;
            return Ok(if settled {
                Settle::Unchanged
            } else {
                Settle::Place
            });
        }
        Ok(match disk {
            OnDisk::Missing => Settle::Place,
            OnDisk::Matches => Settle::Unchanged,
            OnDisk::Differs => match deployed {
                // Changed since this default was deployed: the change is the point.
                Some(previous) if previous == blob => Settle::Unchanged,
                // Changed since an older default was deployed: keep it rather than revert it.
                Some(previous) if on_disk(&file, previous)? == OnDisk::Differs => Settle::Keep,
                // An untouched older default, or a file MSBE never deployed: replace it.
                _ => Settle::Place,
            },
        })
    }

    /// Directories MSBE created, and nothing in `target` needs, that hold nothing once
    /// `removing` is gone. Deepest first, so each is empty by the time it is removed. A
    /// directory with anything unmanaged inside is kept.
    fn prunable_dirs(
        &self,
        target: &BTreeMap<RelPath, Digest>,
        removing: &BTreeSet<RelPath>,
    ) -> Result<Vec<RelPath>, InstanceError> {
        let needed: BTreeSet<RelPath> = target.keys().flat_map(RelPath::ancestors).collect();
        let mut candidates: Vec<RelPath> = self
            .applier
            .journal()
            .created_dirs()
            .into_iter()
            .filter(|dir| !needed.contains(dir))
            .collect();
        candidates.sort_by_key(|dir| Reverse(dir.ancestors().len()));
        let mut pruned = Vec::new();
        let mut gone = BTreeSet::new();
        for dir in candidates {
            if self.empties(&dir, removing, &gone)? {
                gone.insert(dir.clone());
                pruned.push(dir);
            }
        }
        Ok(pruned)
    }

    /// Whether `dir` exists and holds only files in `removing` and directories in `gone`.
    fn empties(
        &self,
        dir: &RelPath,
        removing: &BTreeSet<RelPath>,
        gone: &BTreeSet<RelPath>,
    ) -> Result<bool, InstanceError> {
        let path = dir.to_path(&self.config.root);
        let entries = match fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(false);
            }
            Err(source) => {
                return Err(InstanceError::Io {
                    op: "list",
                    path,
                    source,
                });
            }
        };
        for entry in entries {
            let entry = entry.map_err(io_error("list", &path))?;
            let child = entry
                .file_name()
                .to_str()
                .and_then(|name| RelPath::new(&format!("{dir}/{name}")).ok());
            if !child.is_some_and(|child| removing.contains(&child) || gone.contains(&child)) {
                return Ok(false);
            }
        }
        Ok(true)
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

    /// A profile cannot be removed while it is deployed.
    #[error("profile {0} is currently deployed; deploy another profile or purge first")]
    ProfileDeployed(Name),

    /// No profile has this name.
    #[error("no profile named {0}")]
    UnknownProfile(Name),

    /// No bisection session is active for this instance.
    #[error("no bisection session is active")]
    NoBisect,

    /// Bisection state could not be advanced.
    #[error(transparent)]
    Bisect(#[from] BisectError),

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

    /// The plan does not declare this component.
    #[error("plan does not declare component {0:?}")]
    UnknownComponent(String),

    /// The selected loader does not use this component.
    #[error("loader {loader:?} does not use component {component:?}")]
    UnexpectedComponent {
        /// The selected loader.
        loader: String,
        /// The supplied component.
        component: String,
    },

    /// A component's supplied version or digest differs from the reviewed plan declaration.
    #[error("component {0:?} does not match its reviewed version or SHA-512")]
    ComponentMismatch(String),

    /// The loader bootstrap has not been supplied for this profile.
    #[error("loader bootstrap component {0:?} is not installed for this profile")]
    MissingComponent(String),

    /// A verified component bundle omitted a file its plan declaration requires.
    #[error("component {component:?} is missing declared file {path:?}")]
    MissingComponentFile {
        /// The component.
        component: String,
        /// The bundle path.
        path: String,
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
    /// The selected loader does not support the requested game side.
    #[error("loader {loader:?} does not support the {side:?} target")]
    UnsupportedSide {
        /// The loader id.
        loader: String,
        /// The requested game side.
        side: Side,
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

/// The single blob each path resolves to, or every path claimed with different contents.
fn settle_claims(
    claims: BTreeMap<RelPath, Vec<Claim>>,
) -> Result<BTreeMap<RelPath, Digest>, InstanceError> {
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
    if conflicts.is_empty() {
        Ok(target)
    } else {
        Err(InstanceError::Conflicts(conflicts))
    }
}

/// What deploying one file needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settle {
    /// The file is already as it should be.
    Unchanged,
    /// A mutable file with local changes stays as it is.
    Keep,
    /// The file is placed.
    Place,
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
    use std::{
        collections::{BTreeMap, BTreeSet},
        fs,
        path::PathBuf,
    };

    use msbe_fsops::{
        Backend, Checkpoint, Error as FsError, NoopObserver, Observer, Operation, RelPath,
    };
    use msbe_plan_schema::Side;
    use tempfile::TempDir;

    use super::{
        ComponentEntry, Deployed, DeploymentState, Instance, InstanceError, ModEntry, Name,
        Profile, StoredFile, write_toml,
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
            Self::with_plan(PLAN)
        }

        fn with_plan(text: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let game = dir.path().join("game");
            fs::create_dir_all(game.join("mods")).unwrap();
            fs::write(game.join("mods/existing.bin"), b"vanilla").unwrap();
            let plan = dir.path().join("plan.toml");
            fs::write(&plan, text).unwrap();
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
                    loader_version: None,
                    side: Side::Client,
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
    fn deploys_a_pinned_loader_bootstrap_component() {
        let hash = "0".repeat(128);
        let plan = PLAN.replace("bootstrap = \"none\"", "bootstrap = \"mc.fabric\"")
            + &format!(
                "\n[[components]]\nid = \"mc.fabric\"\nversion = \"0.16.0\"\nsha512 = \"{hash}\"\nfiles = [{{ source = \"profile.json\", path = \"versions/fabric/fabric.json\" }}]\n"
            );
        let fixture = Fixture::with_plan(&plan);
        let mut instance = fixture.create();
        let blob = instance
            .applier
            .store()
            .put_bytes(b"launcher profile")
            .unwrap();
        instance
            .install_component(
                &name("default"),
                "mc.fabric",
                ComponentEntry {
                    version: "0.16.0".to_owned(),
                    sha512: hash,
                    files: vec![StoredFile {
                        source: RelPath::new("profile.json").unwrap(),
                        blob,
                    }],
                },
            )
            .unwrap();

        instance
            .deploy(&name("default"), &mut NoopObserver)
            .unwrap();
        assert_eq!(
            fs::read(fixture.game.join("versions/fabric/fabric.json")).unwrap(),
            b"launcher profile"
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
    fn lockfile_captures_the_portable_resolved_profile() {
        let fixture = Fixture::new();
        let instance = fixture.create();
        let profile = name("default");
        instance
            .add_mods(&profile, &[fixture.input("alpha.bin", b"alpha")])
            .unwrap();

        let lockfile = instance.write_lockfile(&profile).unwrap();
        assert_eq!(lockfile.schema, 1);
        assert_eq!(lockfile.plan.id, "example");
        assert_eq!(lockfile.target.game_version.as_deref(), Some("1.0"));
        assert_eq!(lockfile.target.loader, "loader");
        assert!(lockfile.mods.contains_key(&name("alpha")));
        assert_eq!(lockfile.deployment.len(), 1);
        assert!(
            lockfile
                .deployment
                .contains_key(&RelPath::new("mods/alpha.bin").unwrap())
        );

        let path = fixture
            .home
            .instance(&name("demo"))
            .join("locks/default.toml");
        let persisted: super::Lockfile = super::read_toml(&path).unwrap();
        assert_eq!(persisted, lockfile);
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
                mutable: BTreeSet::new(),
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
            hashes: BTreeMap::from([("sha512".to_owned(), "ab".repeat(64))]),
        };
        instance
            .add_artifacts(
                &default,
                &[super::Artifact {
                    path: fixture.input("download.bin", b"remote"),
                    module: Some(name("remote-mod")),
                    provider: Some(provenance.clone()),
                    source: None,
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
                hashes: BTreeMap::from([("sha512".to_owned(), "ab".repeat(64))]),
            }),
            source: None,
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
    fn legacy_provenance_sha512_migrates_to_the_hash_map() {
        let provenance: super::Provenance = toml::from_str(
            r#"provider = "modrinth"
project = "AANobbMI"
version = "S1"
version_number = "0.8.12"
sha512 = "abc"
"#,
        )
        .unwrap();
        assert_eq!(provenance.hashes.get("sha512"), Some(&"abc".to_owned()));
    }

    #[test]
    fn mutable_files_keep_runtime_changes_and_are_not_drift() {
        let fixture = Fixture::with_plan(&format!(
            "{PLAN}\n[deploy]\nmutable = [\"@loader.targets.mods/*.cfg\"]\n"
        ));
        let mut instance = fixture.create();
        let default = name("default");
        let settings = fixture.game.join("mods/settings.cfg");
        let config_path = || vec![RelPath::new("mods/settings.cfg").unwrap()];
        instance
            .add_mods(
                &default,
                &[
                    fixture.input("settings.cfg", b"default = 1"),
                    fixture.input("alpha.bin", b"alpha"),
                ],
            )
            .unwrap();
        let first = instance.deploy(&default, &mut NoopObserver).unwrap();
        assert_eq!(first.placed, 2);
        assert!(first.backends.contains_key(&Backend::Copy), "{first:?}");

        // The game rewrites its config: that is not drift, and deploying leaves it alone.
        fs::write(&settings, b"default = 1\ntuned = true").unwrap();
        let verified = instance.verify().unwrap();
        assert!(verified.is_clean(), "{verified:?}");
        assert_eq!(verified.changed_at_runtime, config_path());
        let again = instance.deploy(&default, &mut NoopObserver).unwrap();
        assert_eq!((again.placed, again.unchanged), (0, 2));

        // The mod ships a new default: the local changes win, and the deploy says so.
        instance.remove_mod(&default, &name("settings")).unwrap();
        instance
            .add_mods(&default, &[fixture.input("settings.cfg", b"default = 2")])
            .unwrap();
        let updated = instance.deploy(&default, &mut NoopObserver).unwrap();
        assert_eq!(updated.kept, config_path());
        assert_eq!(fs::read(&settings).unwrap(), b"default = 1\ntuned = true");

        // With no local changes left to protect, the new default is placed.
        #[expect(
            clippy::disallowed_methods,
            reason = "simulates the player deleting the file"
        )]
        let deleted = fs::remove_file(&settings);
        deleted.unwrap();
        let replaced = instance.deploy(&default, &mut NoopObserver).unwrap();
        assert_eq!(replaced.placed, 1);
        assert_eq!(fs::read(&settings).unwrap(), b"default = 2");

        instance.purge().unwrap();
        assert!(!settings.exists());
        assert_eq!(
            fs::read(fixture.game.join("mods/existing.bin")).unwrap(),
            b"vanilla"
        );
    }

    #[test]
    fn directories_msbe_created_are_pruned_once_nothing_needs_them() {
        let fixture = Fixture::with_plan(&PLAN.replace("flatten = true", "flatten = false"));
        let mut instance = fixture.create();
        let default = name("default");
        let empty = name("empty");
        let blob = instance.applier.store().put_bytes(b"texture").unwrap();
        let profile = Profile {
            target: None,
            components: BTreeMap::new(),
            mods: BTreeMap::from([(
                name("pack"),
                ModEntry {
                    origin: "pack.zip".to_owned(),
                    provider: None,
                    files: vec![StoredFile {
                        source: RelPath::new("textures/blocks/stone.png").unwrap(),
                        blob,
                    }],
                },
            )]),
        };
        write_toml(&instance.profile_path(&default), &profile).unwrap();
        instance.create_profile(empty.clone(), None).unwrap();
        let textures = fixture.game.join("mods/textures");

        instance.deploy(&default, &mut NoopObserver).unwrap();
        assert!(textures.join("blocks/stone.png").is_file());
        let switched = instance.deploy(&empty, &mut NoopObserver).unwrap();
        assert_eq!((switched.removed, switched.removed_dirs), (1, 2));
        assert!(!textures.exists());
        assert!(
            fixture.game.join("mods").is_dir(),
            "a vanilla directory went"
        );

        // Rolling back recreates them. A file MSBE does not manage keeps its directory.
        instance.rollback().unwrap();
        assert!(textures.join("blocks/stone.png").is_file());
        fs::write(textures.join("notes.txt"), b"mine").unwrap();
        let plan = instance.plan_deploy(&empty).unwrap();
        let removed_dirs: Vec<&str> = plan
            .operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::RemoveDir { path } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(removed_dirs, ["mods/textures/blocks"]);
        instance.deploy(&empty, &mut NoopObserver).unwrap();

        instance.purge().unwrap();
        assert!(!textures.join("blocks").exists());
        assert_eq!(fs::read(textures.join("notes.txt")).unwrap(), b"mine");
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
                    source: None,
                },
                super::Artifact {
                    path: fixture.inputs.join("does-not-exist.bin"),
                    module: None,
                    provider: None,
                    source: None,
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
            target: None,
            components: BTreeMap::new(),
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
