//! Runtime discovery and ownership of validated game plans.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use msbe_plan_schema::{Plan, ValidationError};
use serde::Serialize;
use thiserror::Error;

/// Player-facing information about one supported game.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Game {
    /// Stable game identifier used by daemon requests.
    pub id: String,
    /// Display name from the plan.
    pub name: String,
    /// Version of the loaded support definition.
    pub support_version: String,
    /// Supported mod loading ecosystems.
    pub loaders: Vec<String>,
}

#[derive(Debug)]
struct LoadedPlan {
    game: Game,
    manifest: PathBuf,
}

/// The daemon-owned set of game support definitions available at runtime.
#[derive(Debug)]
pub struct PlanRegistry {
    root: PathBuf,
    plans: BTreeMap<String, LoadedPlan>,
}

impl PlanRegistry {
    /// Loads every `<root>/<game>/plan.toml` manifest. A missing root is an empty registry.
    ///
    /// # Errors
    ///
    /// Returns the first unreadable or invalid manifest instead of starting with partial support.
    pub fn discover(root: PathBuf) -> Result<Self, RegistryError> {
        let mut registry = Self {
            root,
            plans: BTreeMap::new(),
        };
        let entries = match fs::read_dir(&registry.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(registry),
            Err(source) => {
                return Err(RegistryError::ReadDirectory {
                    path: registry.root.clone(),
                    source,
                });
            }
        };
        let mut manifests = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| RegistryError::ReadDirectory {
                path: registry.root.clone(),
                source,
            })?;
            let manifest = entry.path().join("plan.toml");
            if manifest.is_file() {
                manifests.push(manifest);
            }
        }
        manifests.sort();
        for manifest in manifests {
            registry.load_manifest(&manifest)?;
        }
        Ok(registry)
    }

    /// Returns supported games sorted by their stable identifier.
    pub fn games(&self) -> Vec<Game> {
        self.plans
            .values()
            .map(|loaded| loaded.game.clone())
            .collect()
    }

    /// Loads or replaces `<root>/<game>/plan.toml` after validating it.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is unsafe, or the manifest is unreadable or invalid.
    pub fn load(&mut self, game: &str) -> Result<Game, RegistryError> {
        validate_id(game)?;
        let manifest = self.root.join(game).join("plan.toml");
        let loaded = read_manifest(&manifest)?;
        if loaded.id != game {
            return Err(RegistryError::IdMismatch {
                requested: game.to_owned(),
                declared: loaded.id,
            });
        }
        self.insert(loaded.clone(), manifest);
        Ok(loaded)
    }

    /// Unloads one game support definition, returning whether it was present.
    pub fn unload(&mut self, game: &str) -> bool {
        self.plans.remove(game).is_some()
    }

    /// Returns the manifest backing a loaded game.
    pub fn manifest(&self, game: &str) -> Option<&Path> {
        self.plans.get(game).map(|loaded| loaded.manifest.as_path())
    }

    fn load_manifest(&mut self, manifest: &Path) -> Result<Game, RegistryError> {
        let game = read_manifest(manifest)?;
        self.insert(game.clone(), manifest.to_owned());
        Ok(game)
    }

    fn insert(&mut self, game: Game, manifest: PathBuf) {
        self.plans
            .insert(game.id.clone(), LoadedPlan { game, manifest });
    }
}

fn read_manifest(manifest: &Path) -> Result<Game, RegistryError> {
    let source = fs::read_to_string(manifest).map_err(|source| RegistryError::ReadManifest {
        path: manifest.to_owned(),
        source,
    })?;
    let plan: Plan = toml::from_str(&source).map_err(|source| RegistryError::Parse {
        path: manifest.to_owned(),
        source,
    })?;
    plan.validate()
        .map_err(|source| RegistryError::Validation {
            path: manifest.to_owned(),
            source,
        })?;
    let game = Game {
        id: plan.id.clone(),
        name: plan.name.clone(),
        support_version: plan.version.clone(),
        loaders: plan
            .loaders
            .iter()
            .map(|loader| loader.id.clone())
            .collect(),
    };
    Ok(game)
}

fn validate_id(game: &str) -> Result<(), RegistryError> {
    if game.is_empty()
        || !game
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(RegistryError::InvalidId(game.to_owned()));
    }
    Ok(())
}

/// A plan registry discovery or mutation failure.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// The registry directory could not be enumerated.
    #[error("cannot read plan registry directory {path}: {source}")]
    ReadDirectory {
        /// Registry path.
        path: PathBuf,
        /// Filesystem error.
        source: io::Error,
    },
    /// A manifest could not be read.
    #[error("cannot read plan manifest {path}: {source}")]
    ReadManifest {
        /// Manifest path.
        path: PathBuf,
        /// Filesystem error.
        source: io::Error,
    },
    /// A manifest was not valid TOML.
    #[error("cannot parse plan manifest {path}: {source}")]
    Parse {
        /// Manifest path.
        path: PathBuf,
        /// TOML error.
        source: toml::de::Error,
    },
    /// A parsed manifest failed schema validation.
    #[error("invalid plan manifest {path}: {source}")]
    Validation {
        /// Manifest path.
        path: PathBuf,
        /// Validation failure.
        source: ValidationError,
    },
    /// A runtime game identifier may not escape the registry root.
    #[error("invalid game identifier {0}")]
    InvalidId(String),
    /// The directory and manifest identify different games.
    #[error("game {requested} loaded a manifest declaring {declared}")]
    IdMismatch {
        /// Requested directory identifier.
        requested: String,
        /// Manifest identifier.
        declared: String,
    },
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::PlanRegistry;

    const PLAN: &str = r#"
schema = 1
id = "example"
name = "Example Game"
version = "1.0.0"

[[loaders]]
id = "native"
bootstrap = "none"
"#;

    #[test]
    fn discovers_loads_and_unloads_games() {
        let directory = TempDir::new().unwrap();
        let game = directory.path().join("example");
        fs::create_dir(&game).unwrap();
        fs::write(game.join("plan.toml"), PLAN).unwrap();

        let mut registry = PlanRegistry::discover(directory.path().to_owned()).unwrap();
        assert_eq!(
            registry.games().first().map(|game| game.name.as_str()),
            Some("Example Game")
        );
        assert!(registry.manifest("example").is_some());
        assert!(registry.unload("example"));
        assert!(registry.games().is_empty());
        assert_eq!(registry.load("example").unwrap().id, "example");
    }
}
