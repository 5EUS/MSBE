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
    /// The loading regimes supported by the game.
    #[serde(default)]
    pub loaders: Vec<Loader>,
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
        for loader in &self.loaders {
            require_text("loader id", &loader.id)?;
            if !loader_ids.insert(&loader.id) {
                return Err(ValidationError::DuplicateLoader(loader.id.clone()));
            }
            loader.validate()?;
        }

        for step in &self.steps {
            step.validate()?;
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
    fn validate(&self) -> Result<(), ValidationError> {
        require_text("loader bootstrap", &self.bootstrap)?;
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
}

impl Step {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Extract(step) => step.validate(),
            Self::Place(step) => step.validate(),
        }
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
    use super::{Hygiene, Loader, NamedPath, PlaceStep, Plan, SCHEMA_VERSION, Side, Step};

    fn minecraft_plan() -> Plan {
        Plan {
            schema: SCHEMA_VERSION,
            id: "minecraft".to_owned(),
            name: "Minecraft".to_owned(),
            version: "1.0.0".to_owned(),
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
            steps: vec![
                Step::Extract(super::ExtractStep {
                    allow: vec!["**/*.jar".to_owned()],
                    deny: Vec::new(),
                    quarantine: Vec::new(),
                    hygiene: Hygiene::Default,
                }),
                Step::Place(PlaceStep {
                    into: "@loader.targets.mods".to_owned(),
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
    fn rejects_unknown_deployment_target_namespaces() {
        let mut plan = minecraft_plan();
        plan.steps.clear();
        plan.steps.push(Step::Place(PlaceStep {
            into: "mods".to_owned(),
            flatten: false,
        }));
        assert!(plan.validate().is_err());
    }
}
