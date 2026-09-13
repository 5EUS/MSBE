//! Plan manifest types and validation.
//!
//! The single source of truth for the plan schema. Shared with the registry's CI so
//! `msbe plan validate` and the registry gate agree by construction.
//!
//! See `docs/02-plan-system.md`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The only plan schema version understood by this release.
pub const SCHEMA_VERSION: u32 = 1;

/// The placeholder a path or value template may use for the instance's game version.
pub const GAME_VERSION: &str = "{game_version}";

/// A versioned, declarative description of how to install mods for one game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// The manifest schema version.
    pub schema: u32,
    /// A stable, registry-wide plan identifier.
    pub id: String,
    /// The game name displayed to users.
    pub name: String,
    /// The plan's own version.
    pub version: String,
    /// Installation files that distinguish compatible game editions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<InstallationFingerprint>,
    /// Editions of the game that change which mods fit an installation, such as a remaster sold
    /// alongside the original. An instance names at most one; empty means the game has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editions: Vec<Variant>,
    /// Storefronts whose builds change which mods fit an installation, such as a store that ships
    /// a sandboxed build. An instance names at most one; empty means builds do not differ.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storefronts: Vec<Variant>,
    /// Installation-owned inputs that deployment derivations may read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment: Vec<EnvironmentInput>,
    /// Sandboxed WebAssembly modules the plan's `run-extension` steps may run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<ExtensionDeclaration>,
    /// How resolved files are deployed.
    #[serde(default)]
    pub deploy: Deploy,
    /// The loading regimes supported by the game.
    #[serde(default)]
    pub loaders: Vec<Loader>,
    /// Pinned component bundles available to loaders and resolution steps.
    #[serde(default)]
    pub components: Vec<Component>,
    /// The ordered, closed set of resolution steps.
    #[serde(default)]
    pub steps: Vec<Step>,
}

impl Plan {
    /// Validates the manifest invariants that are independent of an instance.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] when the plan cannot be safely resolved.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.schema != SCHEMA_VERSION {
            return Err(ValidationError::UnsupportedSchema(self.schema));
        }
        require_text("plan id", &self.id)?;
        require_text("plan name", &self.name)?;
        require_text("plan version", &self.version)?;
        let mut loader_ids = BTreeSet::new();
        let mut component_ids = BTreeSet::new();
        for component in &self.components {
            component.validate()?;
            if !component_ids.insert(&component.id) {
                return Err(ValidationError::DuplicateComponent(component.id.clone()));
            }
        }

        let editions = validate_variants(EDITION, &self.editions)?;
        let storefronts = validate_variants(STOREFRONT, &self.storefronts)?;
        for loader in &self.loaders {
            require_text("loader id", &loader.id)?;
            if !loader_ids.insert(&loader.id) {
                return Err(ValidationError::DuplicateLoader(loader.id.clone()));
            }
            loader.validate(&component_ids)?;
            for (kind, limit, declared) in [
                (EDITION, &loader.editions, &editions),
                (STOREFRONT, &loader.storefronts, &storefronts),
            ] {
                if let Some(unknown) = limit.iter().find(|id| !declared.contains(id)) {
                    return Err(ValidationError::UnknownLoaderVariant {
                        loader: loader.id.clone(),
                        kind,
                        id: unknown.clone(),
                    });
                }
            }
        }

        if let Some(fingerprint) = &self.fingerprint {
            fingerprint.validate(&loader_ids)?;
        }
        let mut environment_ids = BTreeSet::new();
        for input in &self.environment {
            input.validate(&loader_ids)?;
            if !environment_ids.insert(&input.id) {
                return Err(ValidationError::DuplicateEnvironment(input.id.clone()));
            }
        }

        let mut extension_ids = BTreeSet::new();
        for extension in &self.extensions {
            extension.validate()?;
            if !extension_ids.insert(&extension.id) {
                return Err(ValidationError::DuplicateExtension(extension.id.clone()));
            }
        }

        let mut run_steps = BTreeSet::new();
        for step in &self.steps {
            step.validate(&loader_ids, &extension_ids)?;
            if let Step::RunExtension(run) = step
                && !run_steps.insert(&run.id)
            {
                return Err(ValidationError::DuplicateStep(run.id.clone()));
            }
        }
        self.deploy.validate(&self.loaders)
    }

    /// Checks an installation's edition and storefront: each must be one the plan declares, and one
    /// `loader` supports. An installation that names neither is accepted, since nothing is known
    /// against it.
    ///
    /// # Errors
    ///
    /// Returns [`InstallationError`] naming the first value the plan or the loader rules out.
    pub fn check_installation(
        &self,
        loader: &Loader,
        edition: Option<&str>,
        storefront: Option<&str>,
    ) -> Result<(), InstallationError> {
        for (kind, value, declared, limit) in [
            (EDITION, edition, &self.editions, &loader.editions),
            (
                STOREFRONT,
                storefront,
                &self.storefronts,
                &loader.storefronts,
            ),
        ] {
            let Some(value) = value else {
                continue;
            };
            if !declared.iter().any(|variant| variant.id == value) {
                return Err(InstallationError::Unknown {
                    plan: self.id.clone(),
                    kind,
                    id: value.to_owned(),
                });
            }
            if !admits(limit, value) {
                return Err(InstallationError::Unsupported {
                    loader: loader.id.clone(),
                    kind,
                    id: value.to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// What an edition is called in messages.
const EDITION: &str = "edition";
/// What a storefront is called in messages.
const STOREFRONT: &str = "storefront";

/// One edition or storefront a plan declares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    /// The stable identifier instances and provider programs use, such as `special`.
    pub id: String,
    /// The name displayed to users.
    pub name: String,
}

/// Validates one kind of variant list and returns its ids.
fn validate_variants<'a>(
    kind: &'static str,
    variants: &'a [Variant],
) -> Result<BTreeSet<&'a String>, ValidationError> {
    let mut ids = BTreeSet::new();
    for variant in variants {
        validate_identifier(kind, &variant.id)?;
        require_text(kind, &variant.name)?;
        if !ids.insert(&variant.id) {
            return Err(ValidationError::DuplicateVariant {
                kind,
                id: variant.id.clone(),
            });
        }
    }
    Ok(ids)
}

/// Why an installation's edition or storefront does not fit a plan.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InstallationError {
    /// The plan does not declare the value.
    #[error("plan {plan} declares no {kind} {id:?}")]
    Unknown {
        /// The plan.
        plan: String,
        /// `edition` or `storefront`.
        kind: &'static str,
        /// The value.
        id: String,
    },
    /// The loader is limited to other values.
    #[error("loader {loader} does not support {kind} {id:?}")]
    Unsupported {
        /// The loader.
        loader: String,
        /// `edition` or `storefront`.
        kind: &'static str,
        /// The value.
        id: String,
    },
}

/// Installation identity fields a plan requires before it can reproduce environment-bound output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationFingerprint {
    /// Detected edition or store variant.
    pub edition: String,
    /// Instance-relative identifying paths. Paths may use `{game_version}`.
    pub identifying: Vec<String>,
    /// The loaders whose deployments depend on the installation. Empty means every loader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loaders: Vec<String>,
}

impl InstallationFingerprint {
    /// Whether a lockfile for the loader `id` pins this fingerprint.
    pub fn applies_to(&self, id: &str) -> bool {
        admits(&self.loaders, id)
    }

    fn validate(&self, loaders: &BTreeSet<&String>) -> Result<(), ValidationError> {
        require_text("fingerprint edition", &self.edition)?;
        validate_input_loaders(&self.loaders, loaders)?;
        let mut paths = BTreeSet::new();
        for path in &self.identifying {
            validate_template_path(path)?;
            if !paths.insert(path) {
                return Err(ValidationError::DuplicateFingerprintPath(path.clone()));
            }
        }
        Ok(())
    }
}

/// A named file supplied by the user's installation rather than acquired or embedded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentInput {
    /// Stable root name exposed in portable locks and exports.
    pub id: String,
    /// Instance-relative source path. It may use `{game_version}`.
    pub path: String,
    /// The loaders whose derivations read this input. Empty means every loader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loaders: Vec<String>,
}

impl EnvironmentInput {
    /// Whether a lockfile for the loader `id` records this input.
    pub fn applies_to(&self, id: &str) -> bool {
        admits(&self.loaders, id)
    }

    fn validate(&self, loaders: &BTreeSet<&String>) -> Result<(), ValidationError> {
        require_text("environment input id", &self.id)?;
        validate_input_loaders(&self.loaders, loaders)?;
        validate_template_path(&self.path)
    }
}

/// Whether a `loaders` limit admits the loader `id`. An empty limit admits every loader.
fn admits(limit: &[String], id: &str) -> bool {
    limit.is_empty() || limit.iter().any(|loader| loader == id)
}

fn validate_input_loaders(
    limit: &[String],
    loaders: &BTreeSet<&String>,
) -> Result<(), ValidationError> {
    match limit.iter().find(|id| !loaders.contains(id)) {
        Some(unknown) => Err(ValidationError::UnknownInputLoader(unknown.clone())),
        None => Ok(()),
    }
}

/// The prefix that addresses a loader's deployment targets.
const TARGET_PREFIX: &str = "@loader.targets.";

/// Deployment settings that apply to every placed file. Stored as the `[deploy]` table.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deploy {
    /// Globs over deployed, instance-relative paths that the game or its mods rewrite at
    /// runtime, such as `config/**`. A glob may start with `@loader.targets.<name>`, which
    /// stands for that target's directory under the instance's loader. Matching files are
    /// always copied, never linked, and runtime changes to them are not drift.
    #[serde(default)]
    pub mutable: Vec<String>,
}

impl Deploy {
    fn validate(&self, loaders: &[Loader]) -> Result<(), ValidationError> {
        for pattern in &self.mutable {
            let invalid = |reason: &'static str| ValidationError::InvalidMutablePattern {
                pattern: pattern.clone(),
                reason,
            };
            let glob = match pattern.strip_prefix(TARGET_PREFIX) {
                Some(reference) => {
                    let (name, rest) = reference
                        .split_once('/')
                        .map_or((reference, None), |(name, rest)| (name, Some(rest)));
                    let declared = loaders
                        .iter()
                        .any(|loader| loader.targets.iter().any(|target| target.name == name));
                    if !declared {
                        return Err(invalid("it names a target no loader declares"));
                    }
                    rest
                }
                None if pattern.starts_with('@') => {
                    return Err(invalid(
                        "only @loader.targets.<name> references are supported",
                    ));
                }
                None => Some(pattern.as_str()),
            };
            if glob.is_some_and(|glob| !is_relative_path(glob)) {
                return Err(invalid(
                    "it must be a relative path without empty, '.' or '..' parts, '\\' or ':'",
                ));
            }
        }
        Ok(())
    }
}

/// A mod loading regime available for a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Loader {
    /// The stable loader identifier, such as a mod loader's name, or `none`.
    pub id: String,
    /// Virtual loader APIs this loader satisfies.
    #[serde(default)]
    pub provides: Vec<String>,
    /// The component that establishes this loading regime, or `none`.
    pub bootstrap: String,
    /// Directories addressed by `@loader.targets.<name>` references.
    #[serde(default)]
    pub targets: Vec<NamedPath>,
    /// The sides this loader supports.
    #[serde(default)]
    pub sides: Vec<Side>,
    /// The editions this loader supports, such as a script extender built for one edition. Empty
    /// means every edition.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub editions: Vec<String>,
    /// The storefronts whose builds this loader supports. Empty means every storefront.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub storefronts: Vec<String>,
}

impl Loader {
    fn validate(&self, components: &BTreeSet<&String>) -> Result<(), ValidationError> {
        require_text("loader bootstrap", &self.bootstrap)?;
        if !components.is_empty()
            && self.bootstrap != "none"
            && !components.contains(&self.bootstrap)
        {
            return Err(ValidationError::UnknownComponent(self.bootstrap.clone()));
        }
        let mut names = BTreeSet::new();
        for target in &self.targets {
            target.validate()?;
            if !names.insert(&target.name) {
                return Err(ValidationError::DuplicateTarget(target.name.clone()));
            }
        }
        Ok(())
    }
}

/// A content-addressed, non-executable bundle installed by a reviewed component adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Component {
    /// Stable registry-wide component identity.
    pub id: String,
    /// Exact component version the plan author reviewed.
    pub version: String,
    /// Lowercase SHA-512 of the acquired component bundle.
    pub sha512: String,
    /// Vetted mappings from bundle entries to instance-relative paths.
    pub files: Vec<ComponentFile>,
}

impl Component {
    fn validate(&self) -> Result<(), ValidationError> {
        require_text("component id", &self.id)?;
        require_text("component version", &self.version)?;
        if self.sha512.len() != 128 || !self.sha512.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ValidationError::InvalidComponentHash(self.id.clone()));
        }
        for file in &self.files {
            file.validate()?;
        }
        Ok(())
    }
}

/// One file a component bundle may materialize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentFile {
    /// Safe relative path inside the acquired bundle.
    pub source: String,
    /// Safe instance-relative materialization destination.
    pub path: String,
}

impl ComponentFile {
    fn validate(&self) -> Result<(), ValidationError> {
        if !is_relative_path(&self.source) || !is_relative_path(&self.path) {
            return Err(ValidationError::InvalidPath(format!(
                "{} -> {}",
                self.source, self.path
            )));
        }
        Ok(())
    }
}

/// A named, instance-relative directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedPath {
    /// The symbolic name used in a target reference.
    pub name: String,
    /// The platform-independent relative directory.
    pub path: String,
}

impl NamedPath {
    fn validate(&self) -> Result<(), ValidationError> {
        require_text("target name", &self.name)?;
        if !is_relative_path(&self.path) {
            return Err(ValidationError::InvalidPath(self.path.clone()));
        }
        Ok(())
    }
}

/// The side of a game a loader or artifact supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// A player client.
    Client,
    /// A dedicated server.
    Server,
}

/// A closed plan step whose resolution produces inert operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "with", rename_all = "kebab-case")]
pub enum Step {
    /// Filters an archive tree while retaining excluded files in the content store.
    Extract(ExtractStep),
    /// Places resolved mod artifacts into a declared deployment target.
    Place(PlaceStep),
    /// Merges matching config files from selected artifacts and the user's override layer.
    MergeConfig(MergeConfigStep),
    /// Builds a container from a file in the game and the profile's mods, in profile order.
    Inject(InjectStep),
    /// Derives a JSON document from a file in the game by editing it at JSON pointers.
    EditJson(EditJsonStep),
    /// Runs a sandboxed WebAssembly extension over each mod and deploys what it returns.
    RunExtension(RunExtensionStep),
}

impl Step {
    /// The loaders this step is limited to. Empty means every loader.
    pub fn loaders(&self) -> &[String] {
        match self {
            Self::Extract(step) => &step.loaders,
            Self::Place(step) => &step.loaders,
            Self::MergeConfig(step) => &step.loaders,
            Self::Inject(step) => &step.loaders,
            Self::EditJson(step) => &step.loaders,
            Self::RunExtension(step) => &step.loaders,
        }
    }

    /// Whether this step applies to the loader `id`.
    pub fn applies_to(&self, id: &str) -> bool {
        admits(self.loaders(), id)
    }

    fn validate(
        &self,
        loaders: &BTreeSet<&String>,
        extensions: &BTreeSet<&String>,
    ) -> Result<(), ValidationError> {
        if let Some(unknown) = self.loaders().iter().find(|id| !loaders.contains(id)) {
            return Err(ValidationError::UnknownStepLoader(unknown.clone()));
        }
        match self {
            Self::Extract(step) => step.validate(),
            Self::Place(step) => step.validate(),
            Self::MergeConfig(step) => step.validate(),
            Self::Inject(step) => step.validate(),
            Self::EditJson(step) => step.validate(),
            Self::RunExtension(step) => step.validate(extensions),
        }
    }
}

/// The structured syntax of a configuration file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigFormat {
    /// JSON documents.
    Json,
    /// JSON5 documents, emitted as standard JSON after merging.
    Json5,
    /// TOML documents.
    Toml,
    /// Java `.properties` documents.
    Properties,
}

/// A config source path merged into one instance-relative destination.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeConfigStep {
    /// Loaders this step is limited to. Empty means every loader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loaders: Vec<String>,
    /// The exact artifact-relative source path to merge.
    pub source: String,
    /// The instance-relative destination of the merged document.
    pub into: String,
    /// The parser and deterministic emitter to use.
    pub format: ConfigFormat,
}

impl MergeConfigStep {
    fn validate(&self) -> Result<(), ValidationError> {
        if !is_relative_path(&self.source) || !is_relative_path(&self.into) {
            return Err(ValidationError::InvalidPath(format!(
                "{} -> {}",
                self.source, self.into
            )));
        }
        Ok(())
    }
}

/// Archive filtering rules used by an [`Step::Extract`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractStep {
    /// Loaders this step is limited to. Empty means every loader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loaders: Vec<String>,
    /// Glob patterns whose files are eligible for deployment.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Glob patterns which must never be deployed.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Glob patterns retained in the content store but excluded from deployment.
    #[serde(default)]
    pub quarantine: Vec<String>,
    /// Whether the conservative global hygiene rules apply.
    #[serde(default)]
    pub hygiene: Hygiene,
}

impl ExtractStep {
    fn validate(&self) -> Result<(), ValidationError> {
        if self.allow.iter().any(String::is_empty)
            || self.deny.iter().any(String::is_empty)
            || self.quarantine.iter().any(String::is_empty)
        {
            return Err(ValidationError::EmptyPattern);
        }
        Ok(())
    }
}

/// The global hygiene policy applied during archive extraction.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hygiene {
    /// Apply the conservative built-in exclusions for OS, VCS, and debug artifacts.
    #[default]
    Default,
    /// Leave hygiene decisions entirely to the plan's allow and deny rules.
    None,
}

/// Placement settings for a [`Step::Place`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaceStep {
    /// Loaders this step is limited to. Empty means every loader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loaders: Vec<String>,
    /// A symbolic deployment target such as `@loader.targets.mods`.
    pub into: String,
    /// Source glob patterns eligible for this target. Empty selects every source file.
    #[serde(default)]
    pub include: Vec<String>,
    /// Source glob patterns excluded from this target after inclusion checks.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// A source-directory prefix removed before placement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_prefix: Option<String>,
    /// Whether the source tree's directory structure is discarded.
    #[serde(default)]
    pub flatten: bool,
}

impl PlaceStep {
    fn validate(&self) -> Result<(), ValidationError> {
        let Some(name) = self.into.strip_prefix("@loader.targets.") else {
            return Err(ValidationError::InvalidTargetReference(self.into.clone()));
        };
        if name.is_empty() || name.contains(['/', '\\', ':']) {
            return Err(ValidationError::InvalidTargetReference(self.into.clone()));
        }
        if self.include.iter().any(String::is_empty) || self.exclude.iter().any(String::is_empty) {
            return Err(ValidationError::EmptyPattern);
        }
        if self
            .strip_prefix
            .as_deref()
            .is_some_and(|prefix| !is_relative_path(prefix))
        {
            return Err(ValidationError::InvalidPath(
                self.strip_prefix.clone().unwrap_or_default(),
            ));
        }
        Ok(())
    }
}

/// Container injection settings for a [`Step::Inject`], such as mods patched into a game's own archive.
///
/// The container starts as `base` was before MSBE changed anything. Every mod's files are then
/// written into it in profile order, so a later mod wins an entry two mods ship, and the
/// entries `remove` matches are left out. The result is placed at `into`, which may be `base`
/// itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectStep {
    /// Stable identifier used to pin this transform in lockfiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Loaders this step is limited to. Empty means every loader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loaders: Vec<String>,
    /// The instance-relative zip container the result starts from. May use `{game_version}`.
    pub base: String,
    /// The instance-relative path the result is placed at. May use `{game_version}`.
    pub into: String,
    /// Globs over container entries to leave out, such as `META-INF/**`.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Mod source globs to inject. Empty injects every source file.
    #[serde(default)]
    pub include: Vec<String>,
}

impl InjectStep {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(id) = &self.id {
            require_text("inject step id", id)?;
        }
        validate_template_path(&self.base)?;
        validate_template_path(&self.into)?;
        if self.remove.iter().any(String::is_empty) || self.include.iter().any(String::is_empty) {
            return Err(ValidationError::EmptyPattern);
        }
        Ok(())
    }
}

/// JSON derivation settings for a [`Step::EditJson`], such as a launcher version manifest.
///
/// The document starts as `base` was before MSBE changed anything. Each pointer in `remove` is
/// deleted, each pointer in `set` is set to its string, and the result is placed at `into`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditJsonStep {
    /// Stable identifier used to pin this transform in lockfiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Loaders this step is limited to. Empty means every loader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loaders: Vec<String>,
    /// The instance-relative JSON document the result starts from. May use `{game_version}`.
    pub base: String,
    /// The instance-relative path the result is placed at. May use `{game_version}`.
    pub into: String,
    /// JSON pointers, such as `/id`, and the strings they are set to. Values may use
    /// `{game_version}`.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// JSON pointers to delete, such as `/downloads/client`.
    #[serde(default)]
    pub remove: Vec<String>,
}

impl EditJsonStep {
    fn validate(&self) -> Result<(), ValidationError> {
        if let Some(id) = &self.id {
            require_text("edit-json step id", id)?;
        }
        validate_template_path(&self.base)?;
        validate_template_path(&self.into)?;
        for pointer in self.set.keys().chain(&self.remove) {
            if !pointer.starts_with('/') {
                return Err(ValidationError::InvalidJsonPointer(pointer.clone()));
            }
        }
        if let Some(value) = self.set.values().find(|value| !is_template(value)) {
            return Err(ValidationError::InvalidTemplate(value.clone()));
        }
        Ok(())
    }
}

/// A sandboxed WebAssembly module that `run-extension` steps may run, and what it is granted.
///
/// Anything not granted here does not exist in the module's world: the host links only the imports
/// these capabilities name, so a module that asks for any other import fails to load. See
/// `docs/18-wasm-extensions.md`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionDeclaration {
    /// Stable identifier that steps use to name the extension.
    pub id: String,
    /// The module file, relative to the directory that holds the plan manifest.
    pub path: String,
    /// Lowercase hexadecimal SHA-256 of the module bytes, which pins them.
    pub sha256: String,
    /// Host capabilities the module may use.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<ExtensionCapability>,
    /// Instance-relative globs that the `game-read` capability may read. Required with that
    /// capability and refused without it. Globs may use `{game_version}`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub game_read: Vec<String>,
    /// The operation kinds the module may return.
    pub emit: Vec<EmitKind>,
    /// Where returned operations may write: `@loader.targets.<name>` references or
    /// instance-relative directories. Empty means every target of the selected loader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
}

impl ExtensionDeclaration {
    /// Whether the module is granted `capability`.
    pub fn grants(&self, capability: ExtensionCapability) -> bool {
        self.capabilities.contains(&capability)
    }

    fn validate(&self) -> Result<(), ValidationError> {
        validate_identifier("extension id", &self.id)?;
        if !is_relative_path(&self.path) {
            return Err(ValidationError::InvalidPath(self.path.clone()));
        }
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(ValidationError::InvalidExtensionHash(self.id.clone()));
        }
        let grant = |reason| ValidationError::InvalidGrant {
            extension: self.id.clone(),
            reason,
        };
        if has_duplicates(&self.capabilities) {
            return Err(grant("a capability is listed more than once"));
        }
        if self.grants(ExtensionCapability::GameRead) == self.game_read.is_empty() {
            return Err(grant(
                "`game_read` globs are required with the game-read capability, and only with it",
            ));
        }
        for glob in &self.game_read {
            validate_template_path(glob)?;
        }
        if self.emit.is_empty() || has_duplicates(&self.emit) {
            return Err(grant(
                "`emit` must list each operation kind once, and at least one",
            ));
        }
        for root in &self.roots {
            let valid = match root.strip_prefix(TARGET_PREFIX) {
                Some(name) => !name.is_empty() && !name.contains(['/', '\\', ':']),
                None => is_relative_path(root),
            };
            if !valid {
                return Err(ValidationError::InvalidTargetReference(root.clone()));
            }
        }
        Ok(())
    }
}

/// A host capability a step extension may be granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionCapability {
    /// List and read the files of the mod the step runs over.
    ArchiveRead,
    /// Read game files that match the declaration's `game_read` globs.
    GameRead,
    /// Ask installer questions, answered from the mod's recorded answers.
    UiPrompt,
}

impl ExtensionCapability {
    /// The capability's manifest spelling, such as `archive-read`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ArchiveRead => "archive-read",
            Self::GameRead => "game-read",
            Self::UiPrompt => "ui-prompt",
        }
    }
}

/// An operation kind a step extension may return.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EmitKind {
    /// Place one of the mod's files at an instance-relative path.
    Place,
    /// Write generated text to an instance-relative path.
    WriteFile,
}

impl EmitKind {
    /// The operation kind's manifest spelling, such as `write-file`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Place => "place",
            Self::WriteFile => "write-file",
        }
    }
}

/// Settings for a [`Step::RunExtension`], which runs a declared extension over each mod.
///
/// The extension's only effect is the operations it returns. The host checks each one against the
/// extension's declaration before core turns it into a deployment claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunExtensionStep {
    /// Stable identifier. It keys a mod's recorded installer answers and pins the transform in
    /// lockfiles, so it may use only ASCII letters, digits, `-` and `_`.
    pub id: String,
    /// Loaders this step is limited to. Empty means every loader.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loaders: Vec<String>,
    /// The declared extension to run.
    pub extension: String,
    /// String parameters passed to the extension. Values may use `{game_version}`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub parameters: BTreeMap<String, String>,
}

impl RunExtensionStep {
    fn validate(&self, extensions: &BTreeSet<&String>) -> Result<(), ValidationError> {
        validate_identifier("run-extension step id", &self.id)?;
        if !extensions.contains(&self.extension) {
            return Err(ValidationError::UnknownExtension(self.extension.clone()));
        }
        for (key, value) in &self.parameters {
            validate_identifier("extension parameter", key)?;
            if !is_template(value) {
                return Err(ValidationError::InvalidTemplate(value.clone()));
            }
        }
        Ok(())
    }
}

/// Whether `path` matches the glob `pattern`. Both use `/` separators. `**` matches any number of
/// whole components, `*` any run of characters within one component, and `?` one character.
pub fn matches_glob(pattern: &str, path: &str) -> bool {
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
    glob_chars_match(head.chars(), path_head.chars()) && matches_glob_parts(tail, path_tail)
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

fn has_duplicates<T: Ord>(values: &[T]) -> bool {
    let mut seen = BTreeSet::new();
    !values.iter().all(|value| seen.insert(value))
}

/// Checks that `value` is non-empty and uses only ASCII letters, digits, `-` and `_`.
fn validate_identifier(field: &'static str, value: &str) -> Result<(), ValidationError> {
    require_text(field, value)?;
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(ValidationError::InvalidIdentifier {
            field,
            value: value.to_owned(),
        })
    }
}

/// A manifest invariant that prevents deterministic resolution.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    /// The manifest requires a schema newer or older than this engine supports.
    #[error("unsupported plan schema {0}")]
    UnsupportedSchema(u32),
    /// A required identifying field is empty.
    #[error("{0} must not be empty")]
    EmptyField(&'static str),
    /// Multiple loaders declare the same identifier.
    #[error("duplicate loader {0:?}")]
    DuplicateLoader(String),
    /// Multiple components declare the same identifier.
    #[error("duplicate component {0:?}")]
    DuplicateComponent(String),
    /// A loader refers to an undeclared bootstrap component.
    #[error("unknown component {0:?}")]
    UnknownComponent(String),
    /// A component's declared SHA-512 is not 128 hexadecimal characters.
    #[error("component {0:?} has an invalid SHA-512")]
    InvalidComponentHash(String),
    /// Multiple deployment targets on one loader have the same name.
    #[error("duplicate loader target {0:?}")]
    DuplicateTarget(String),
    /// Multiple environment inputs use the same stable ID.
    #[error("duplicate environment input {0:?}")]
    DuplicateEnvironment(String),
    /// An installation fingerprint repeats one identifying path.
    #[error("duplicate fingerprint path {0:?}")]
    DuplicateFingerprintPath(String),
    /// An installation fingerprint or environment input names an undeclared loader.
    #[error("installation input refers to unknown loader {0:?}")]
    UnknownInputLoader(String),
    /// Multiple extensions declare the same identifier.
    #[error("duplicate extension {0:?}")]
    DuplicateExtension(String),
    /// Multiple `run-extension` steps share an identifier.
    #[error("duplicate run-extension step {0:?}")]
    DuplicateStep(String),
    /// A step names an extension the plan does not declare.
    #[error("unknown extension {0:?}")]
    UnknownExtension(String),
    /// An extension's SHA-256 is not 64 lowercase hexadecimal characters.
    #[error("extension {0:?} has an invalid SHA-256")]
    InvalidExtensionHash(String),
    /// An extension's grant is inconsistent.
    #[error("extension {extension:?}: {reason}")]
    InvalidGrant {
        /// The extension.
        extension: String,
        /// What is inconsistent.
        reason: &'static str,
    },
    /// An identifier uses characters other than ASCII letters, digits, `-` and `_`.
    #[error("{field} {value:?} may use only ASCII letters, digits, '-' and '_'")]
    InvalidIdentifier {
        /// Which identifier.
        field: &'static str,
        /// The refused value.
        value: String,
    },
    /// A deployment path is not platform-independent and instance-relative.
    #[error("invalid instance-relative path {0:?}")]
    InvalidPath(String),
    /// A step addressed a target outside the loader target namespace.
    #[error("invalid deployment target reference {0:?}")]
    InvalidTargetReference(String),
    /// A glob rule was blank.
    #[error("archive filter patterns must not be empty")]
    EmptyPattern,
    /// A mutable path pattern is malformed.
    #[error("invalid mutable path pattern {pattern:?}: {reason}")]
    InvalidMutablePattern {
        /// The pattern.
        pattern: String,
        /// The rule it broke.
        reason: &'static str,
    },
    /// A step is limited to a loader the plan does not declare.
    #[error("a step is limited to loader {0:?}, which the plan does not declare")]
    UnknownStepLoader(String),
    /// Two editions, or two storefronts, share an identifier.
    #[error("duplicate {kind} {id:?}")]
    DuplicateVariant {
        /// `edition` or `storefront`.
        kind: &'static str,
        /// The repeated identifier.
        id: String,
    },
    /// A loader is limited to an edition or storefront the plan does not declare.
    #[error("loader {loader:?} is limited to {kind} {id:?}, which the plan does not declare")]
    UnknownLoaderVariant {
        /// The loader.
        loader: String,
        /// `edition` or `storefront`.
        kind: &'static str,
        /// The undeclared identifier.
        id: String,
    },
    /// A template uses a placeholder other than `{game_version}`.
    #[error("invalid template {0:?}: only {{game_version}} may appear in braces")]
    InvalidTemplate(String),
    /// A JSON pointer does not start with `/`.
    #[error("invalid JSON pointer {0:?}: it must start with '/'")]
    InvalidJsonPointer(String),
}

fn require_text(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        Err(ValidationError::EmptyField(field))
    } else {
        Ok(())
    }
}

/// Whether `path` is a safe, platform-independent relative path: `/`-separated, with no empty,
/// `.` or `..` component, and no backslash, colon or NUL.
pub fn is_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', ':', '\0'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Whether `text` uses no braces other than the `{game_version}` placeholder.
fn is_template(text: &str) -> bool {
    !text.replace(GAME_VERSION, "").contains(['{', '}'])
}

/// Checks a path template: its only placeholder is `{game_version}`, and it is a safe relative
/// path once that is filled in.
fn validate_template_path(template: &str) -> Result<(), ValidationError> {
    if !is_template(template) {
        return Err(ValidationError::InvalidTemplate(template.to_owned()));
    }
    if !is_relative_path(&template.replace(GAME_VERSION, "version")) {
        return Err(ValidationError::InvalidPath(template.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        Deploy, EditJsonStep, EmitKind, EnvironmentInput, ExtensionCapability,
        ExtensionDeclaration, Hygiene, InjectStep, InstallationError, InstallationFingerprint,
        Loader, NamedPath, PlaceStep, Plan, RunExtensionStep, SCHEMA_VERSION, Side, Step,
        ValidationError, Variant, matches_glob,
    };

    fn example_plan() -> Plan {
        Plan {
            schema: SCHEMA_VERSION,
            id: "example".to_owned(),
            name: "Example".to_owned(),
            version: "1.0.0".to_owned(),
            fingerprint: None,
            editions: Vec::new(),
            storefronts: Vec::new(),
            environment: Vec::new(),
            extensions: Vec::new(),
            deploy: Deploy::default(),
            loaders: vec![Loader {
                id: "modern".to_owned(),
                provides: Vec::new(),
                bootstrap: "example.installer".to_owned(),
                targets: vec![NamedPath {
                    name: "mods".to_owned(),
                    path: "mods".to_owned(),
                }],
                sides: vec![Side::Client, Side::Server],
                editions: Vec::new(),
                storefronts: Vec::new(),
            }],
            components: Vec::new(),
            steps: vec![
                Step::Extract(super::ExtractStep {
                    loaders: Vec::new(),
                    allow: vec!["**/*.jar".to_owned()],
                    deny: Vec::new(),
                    quarantine: Vec::new(),
                    hygiene: Hygiene::Default,
                }),
                Step::Place(PlaceStep {
                    loaders: Vec::new(),
                    into: "@loader.targets.mods".to_owned(),
                    include: Vec::new(),
                    exclude: Vec::new(),
                    strip_prefix: None,
                    flatten: true,
                }),
            ],
        }
    }

    fn jarmod_plan() -> Plan {
        let mut plan = example_plan();
        plan.loaders.push(Loader {
            id: "jarmod".to_owned(),
            provides: Vec::new(),
            bootstrap: "none".to_owned(),
            targets: Vec::new(),
            sides: vec![Side::Client],
            editions: Vec::new(),
            storefronts: Vec::new(),
        });
        plan.steps.push(Step::Inject(InjectStep {
            id: Some("inject-client".to_owned()),
            loaders: vec!["jarmod".to_owned()],
            base: "versions/{game_version}/{game_version}.jar".to_owned(),
            into: "versions/{game_version}-msbe/{game_version}-msbe.jar".to_owned(),
            remove: vec!["META-INF/**".to_owned()],
            include: Vec::new(),
        }));
        plan.steps.push(Step::EditJson(EditJsonStep {
            id: Some("edit-client-manifest".to_owned()),
            loaders: vec!["jarmod".to_owned()],
            base: "versions/{game_version}/{game_version}.json".to_owned(),
            into: "versions/{game_version}-msbe/{game_version}-msbe.json".to_owned(),
            set: BTreeMap::from([("/id".to_owned(), "{game_version}-msbe".to_owned())]),
            remove: vec!["/downloads/client".to_owned()],
        }));
        plan
    }

    #[test]
    fn serializes_and_validates_a_modern_example_plan() {
        let plan = example_plan();
        plan.validate().unwrap();
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), plan);
    }

    #[test]
    fn validates_unique_installation_identity_inputs() {
        let mut plan = jarmod_plan();
        plan.fingerprint = Some(InstallationFingerprint {
            edition: "retail".to_owned(),
            identifying: vec!["game/{game_version}.exe".to_owned()],
            loaders: vec!["jarmod".to_owned()],
        });
        let scoped = |loaders: &[&str]| EnvironmentInput {
            id: "game".to_owned(),
            path: "game/{game_version}.exe".to_owned(),
            loaders: loaders.iter().map(|loader| (*loader).to_owned()).collect(),
        };
        plan.environment = vec![scoped(&["jarmod"])];
        plan.validate().unwrap();
        assert!(scoped(&["jarmod"]).applies_to("jarmod"));
        assert!(!scoped(&["jarmod"]).applies_to("modern"));
        assert!(scoped(&[]).applies_to("modern"));

        plan.environment = vec![scoped(&["unknown"])];
        assert_eq!(
            plan.validate(),
            Err(ValidationError::UnknownInputLoader("unknown".to_owned()))
        );
        plan.environment = vec![scoped(&[])];

        plan.environment.push(EnvironmentInput {
            id: "game".to_owned(),
            path: "game/other.exe".to_owned(),
            loaders: Vec::new(),
        });
        assert!(matches!(
            plan.validate(),
            Err(ValidationError::DuplicateEnvironment(id)) if id == "game"
        ));
    }

    fn extension_plan() -> Plan {
        let mut plan = example_plan();
        plan.extensions = vec![ExtensionDeclaration {
            id: "installer".to_owned(),
            path: "extensions/installer.wasm".to_owned(),
            sha256: "0".repeat(64),
            capabilities: vec![
                ExtensionCapability::ArchiveRead,
                ExtensionCapability::GameRead,
            ],
            game_read: vec!["Data/*.esm".to_owned()],
            emit: vec![EmitKind::Place, EmitKind::WriteFile],
            roots: vec!["@loader.targets.mods".to_owned()],
        }];
        plan.steps.push(Step::RunExtension(RunExtensionStep {
            id: "install".to_owned(),
            loaders: Vec::new(),
            extension: "installer".to_owned(),
            parameters: BTreeMap::from([("into".to_owned(), "mods/{game_version}".to_owned())]),
        }));
        plan
    }

    #[test]
    fn run_extension_steps_round_trip_and_name_declared_extensions() {
        let plan = extension_plan();
        plan.validate().unwrap();
        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains(r#""archive-read""#) && json.contains(r#""write-file""#));
        assert!(json.contains(r#""type":"run-extension""#));
        assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), plan);

        let mut steps = plan.clone();
        for step in &mut steps.steps {
            if let Step::RunExtension(run) = step {
                run.extension = "missing".to_owned();
            }
        }
        assert_eq!(
            steps.validate(),
            Err(ValidationError::UnknownExtension("missing".to_owned()))
        );

        let mut repeated = plan.clone();
        let run = repeated.steps.last().cloned().unwrap();
        repeated.steps.push(run);
        assert_eq!(
            repeated.validate(),
            Err(ValidationError::DuplicateStep("install".to_owned()))
        );

        let mut named = plan;
        for step in &mut named.steps {
            if let Step::RunExtension(run) = step {
                run.id = "install/one".to_owned();
            }
        }
        assert!(matches!(
            named.validate(),
            Err(ValidationError::InvalidIdentifier { value, .. }) if value == "install/one"
        ));
    }

    #[test]
    fn extension_grants_must_be_consistent() {
        let edited = |edit: fn(&mut ExtensionDeclaration)| {
            let mut plan = extension_plan();
            plan.extensions.iter_mut().for_each(edit);
            plan.validate()
        };
        assert!(matches!(
            edited(|extension| extension.game_read.clear()),
            Err(ValidationError::InvalidGrant { .. })
        ));
        assert!(matches!(
            edited(|extension| extension.capabilities = vec![ExtensionCapability::ArchiveRead]),
            Err(ValidationError::InvalidGrant { .. })
        ));
        assert!(matches!(
            edited(|extension| extension.emit.clear()),
            Err(ValidationError::InvalidGrant { .. })
        ));
        assert_eq!(
            edited(|extension| extension.sha256 = "A".repeat(64)),
            Err(ValidationError::InvalidExtensionHash(
                "installer".to_owned()
            ))
        );
        assert_eq!(
            edited(|extension| extension.path = "../installer.wasm".to_owned()),
            Err(ValidationError::InvalidPath("../installer.wasm".to_owned()))
        );
        assert!(matches!(
            edited(|extension| extension.roots = vec!["@loader.targets.".to_owned()]),
            Err(ValidationError::InvalidTargetReference(_))
        ));
    }

    #[test]
    fn globs_match_components_and_recursive_directories() {
        assert!(matches_glob("**/*.jar", "release/nested/example.jar"));
        assert!(matches_glob("Data/*.esm", "Data/Base.esm"));
        assert!(!matches_glob("Data/*.esm", "Data/nested/Base.esm"));
        assert!(matches_glob("Data/Bas?.esm", "Data/Base.esm"));
    }

    #[test]
    fn editions_and_storefronts_are_declared_and_limit_loaders() {
        let mut plan = example_plan();
        plan.editions = vec![
            Variant {
                id: "original".to_owned(),
                name: "Original".to_owned(),
            },
            Variant {
                id: "remaster".to_owned(),
                name: "Remaster".to_owned(),
            },
        ];
        plan.storefronts = vec![Variant {
            id: "sandboxed".to_owned(),
            name: "Sandboxed store".to_owned(),
        }];
        if let Some(loader) = plan.loaders.first_mut() {
            loader.editions = vec!["remaster".to_owned()];
        }
        plan.validate().unwrap();
        let loader = plan.loaders.first().unwrap();
        assert!(plan.check_installation(loader, None, None).is_ok());
        assert!(
            plan.check_installation(loader, Some("remaster"), Some("sandboxed"))
                .is_ok()
        );
        assert!(matches!(
            plan.check_installation(loader, Some("original"), None),
            Err(InstallationError::Unsupported {
                kind: "edition",
                ..
            })
        ));
        assert!(matches!(
            plan.check_installation(loader, None, Some("other")),
            Err(InstallationError::Unknown {
                kind: "storefront",
                ..
            })
        ));

        let mut undeclared = plan.clone();
        if let Some(loader) = undeclared.loaders.first_mut() {
            loader.storefronts = vec!["other".to_owned()];
        }
        assert!(matches!(
            undeclared.validate(),
            Err(ValidationError::UnknownLoaderVariant {
                kind: "storefront",
                ..
            })
        ));
        let mut duplicate = plan.clone();
        duplicate.editions.push(Variant {
            id: "original".to_owned(),
            name: "Again".to_owned(),
        });
        assert!(matches!(
            duplicate.validate(),
            Err(ValidationError::DuplicateVariant { .. })
        ));
        let mut malformed = plan;
        malformed.storefronts = vec![Variant {
            id: "game pass".to_owned(),
            name: "Game Pass".to_owned(),
        }];
        assert!(matches!(
            malformed.validate(),
            Err(ValidationError::InvalidIdentifier { .. })
        ));
    }

    #[test]
    fn rejects_unsafe_deployment_paths() {
        let mut plan = example_plan();
        plan.loaders.clear();
        plan.loaders.push(Loader {
            id: "modern".to_owned(),
            provides: Vec::new(),
            bootstrap: "example.installer".to_owned(),
            targets: vec![NamedPath {
                name: "mods".to_owned(),
                path: "../mods".to_owned(),
            }],
            sides: vec![Side::Client],
            editions: Vec::new(),
            storefronts: Vec::new(),
        });
        assert!(plan.validate().is_err());
    }

    #[test]
    fn mutable_patterns_are_relative_and_reference_only_declared_targets() {
        let mut plan = example_plan();
        plan.deploy.mutable = vec![
            "config/**".to_owned(),
            "@loader.targets.mods/*.cfg".to_owned(),
        ];
        plan.validate().unwrap();

        for bad in [
            "",
            "../config/**",
            "/etc/**",
            "config\\x",
            "@loader.targets.config/**",
            "@paths.mods/**",
        ] {
            let mut plan = example_plan();
            plan.deploy.mutable = vec![bad.to_owned()];
            assert!(
                matches!(
                    plan.validate(),
                    Err(ValidationError::InvalidMutablePattern { .. })
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn rejects_unknown_deployment_target_namespaces() {
        let mut plan = example_plan();
        plan.steps.clear();
        plan.steps.push(Step::Place(PlaceStep {
            loaders: Vec::new(),
            into: "mods".to_owned(),
            include: Vec::new(),
            exclude: Vec::new(),
            strip_prefix: None,
            flatten: false,
        }));
        assert!(plan.validate().is_err());
    }

    #[test]
    fn merge_config_paths_must_be_safe_and_relative() {
        let mut plan = example_plan();
        plan.steps.push(Step::MergeConfig(super::MergeConfigStep {
            loaders: Vec::new(),
            source: "config/options.toml".to_owned(),
            into: "config/options.toml".to_owned(),
            format: super::ConfigFormat::Toml,
        }));
        plan.validate().unwrap();
        let Step::MergeConfig(step) = plan.steps.last_mut().unwrap() else {
            panic!("expected merge-config step");
        };
        step.into = "../options.toml".to_owned();
        assert!(plan.validate().is_err());
    }

    #[test]
    fn jarmod_steps_round_trip_and_apply_only_to_their_loaders() {
        let plan = jarmod_plan();
        plan.validate().unwrap();
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), plan);

        let applies: Vec<(bool, bool)> = plan
            .steps
            .iter()
            .map(|step| (step.applies_to("modern"), step.applies_to("jarmod")))
            .collect();
        assert_eq!(
            applies,
            [(true, true), (true, true), (false, true), (false, true)]
        );
    }

    #[test]
    fn jarmod_steps_refuse_unknown_loaders_placeholders_paths_and_pointers() {
        fn inject(plan: &mut Plan) -> &mut InjectStep {
            plan.steps
                .iter_mut()
                .find_map(|step| match step {
                    Step::Inject(inject) => Some(inject),
                    _ => None,
                })
                .unwrap()
        }

        fn edit_json(plan: &mut Plan) -> &mut EditJsonStep {
            plan.steps
                .iter_mut()
                .find_map(|step| match step {
                    Step::EditJson(edit) => Some(edit),
                    _ => None,
                })
                .unwrap()
        }

        let edit = |change: fn(&mut Plan)| {
            let mut plan = jarmod_plan();
            change(&mut plan);
            plan.validate()
        };

        assert_eq!(
            edit(|plan| plan.loaders.retain(|loader| loader.id != "jarmod")),
            Err(ValidationError::UnknownStepLoader("jarmod".to_owned()))
        );
        assert!(matches!(
            edit(|plan| inject(plan).base = "versions/{version}/client.jar".to_owned()),
            Err(ValidationError::InvalidTemplate(_))
        ));
        assert!(matches!(
            edit(|plan| inject(plan).into = "../{game_version}.jar".to_owned()),
            Err(ValidationError::InvalidPath(_))
        ));
        assert_eq!(
            edit(|plan| inject(plan).remove.push(String::new())),
            Err(ValidationError::EmptyPattern)
        );
        assert!(matches!(
            edit(|plan| edit_json(plan).remove.push("downloads".to_owned())),
            Err(ValidationError::InvalidJsonPointer(_))
        ));
        assert!(matches!(
            edit(|plan| {
                edit_json(plan)
                    .set
                    .insert("/id".to_owned(), "{loader}".to_owned());
            }),
            Err(ValidationError::InvalidTemplate(_))
        ));
    }
}
