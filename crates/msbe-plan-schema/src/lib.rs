//! Plan manifest types and validation.
//!
//! The single source of truth for the plan schema. Shared with the registry's CI so
//! `msbe plan validate` and the registry gate agree by construction.
//!
//! See `docs/02-plan-system.md`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The only plan schema version understood by this release.
pub const SCHEMA_VERSION: u32 = 1;

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

        for loader in &self.loaders {
            require_text("loader id", &loader.id)?;
            if !loader_ids.insert(&loader.id) {
                return Err(ValidationError::DuplicateLoader(loader.id.clone()));
            }
            loader.validate(&component_ids)?;
        }

        for step in &self.steps {
            step.validate()?;
        }
        self.deploy.validate(&self.loaders)
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
    /// The stable loader identifier, such as `fabric` or `neoforge`.
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
}

impl Step {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Extract(step) => step.validate(),
            Self::Place(step) => step.validate(),
            Self::MergeConfig(step) => step.validate(),
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
    /// A symbolic deployment target such as `@loader.targets.mods`.
    pub into: String,
    /// Source glob patterns eligible for this target. Empty selects every source file.
    #[serde(default)]
    pub include: Vec<String>,
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
        if self.include.iter().any(String::is_empty) {
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
}

fn require_text(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if value.trim().is_empty() {
        Err(ValidationError::EmptyField(field))
    } else {
        Ok(())
    }
}

fn is_relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains(['\\', ':', '\0'])
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

#[cfg(test)]
mod tests {
    use super::{
        Deploy, Hygiene, Loader, NamedPath, PlaceStep, Plan, SCHEMA_VERSION, Side, Step,
        ValidationError,
    };

    fn minecraft_plan() -> Plan {
        Plan {
            schema: SCHEMA_VERSION,
            id: "minecraft".to_owned(),
            name: "Minecraft".to_owned(),
            version: "1.0.0".to_owned(),
            deploy: Deploy::default(),
            loaders: vec![Loader {
                id: "fabric".to_owned(),
                provides: Vec::new(),
                bootstrap: "mc.fabric-installer".to_owned(),
                targets: vec![NamedPath {
                    name: "mods".to_owned(),
                    path: "mods".to_owned(),
                }],
                sides: vec![Side::Client, Side::Server],
            }],
            components: Vec::new(),
            steps: vec![
                Step::Extract(super::ExtractStep {
                    allow: vec!["**/*.jar".to_owned()],
                    deny: Vec::new(),
                    quarantine: Vec::new(),
                    hygiene: Hygiene::Default,
                }),
                Step::Place(PlaceStep {
                    into: "@loader.targets.mods".to_owned(),
                    include: Vec::new(),
                    strip_prefix: None,
                    flatten: true,
                }),
            ],
        }
    }

    #[test]
    fn serializes_and_validates_a_modern_minecraft_plan() {
        let plan = minecraft_plan();
        plan.validate().unwrap();
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(serde_json::from_str::<Plan>(&json).unwrap(), plan);
    }

    #[test]
    fn rejects_unsafe_deployment_paths() {
        let mut plan = minecraft_plan();
        plan.loaders.clear();
        plan.loaders.push(Loader {
            id: "fabric".to_owned(),
            provides: Vec::new(),
            bootstrap: "mc.fabric-installer".to_owned(),
            targets: vec![NamedPath {
                name: "mods".to_owned(),
                path: "../mods".to_owned(),
            }],
            sides: vec![Side::Client],
        });
        assert!(plan.validate().is_err());
    }

    #[test]
    fn mutable_patterns_are_relative_and_reference_only_declared_targets() {
        let mut plan = minecraft_plan();
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
            let mut plan = minecraft_plan();
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
        let mut plan = minecraft_plan();
        plan.steps.clear();
        plan.steps.push(Step::Place(PlaceStep {
            into: "mods".to_owned(),
            include: Vec::new(),
            strip_prefix: None,
            flatten: false,
        }));
        assert!(plan.validate().is_err());
    }

    #[test]
    fn merge_config_paths_must_be_safe_and_relative() {
        let mut plan = minecraft_plan();
        plan.steps.push(Step::MergeConfig(super::MergeConfigStep {
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
}
