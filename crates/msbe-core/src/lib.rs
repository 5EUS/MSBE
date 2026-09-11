//! Plan engine, solver and the resolve/apply pipeline.
//!
//! Knows nothing about any game. If a game name, store name, loader or file format
//! appears in this crate, that is a design bug: the test is that this crate compiles
//! and passes its suite with zero plans installed.
//!
//! See `docs/00-overview.md` and `docs/02-plan-system.md`.

use msbe_fsops::{Applier, Digest, Observer, Operation, RelPath, Result as FsResult, TxnReport};
use msbe_plan_schema::{Plan, Step, ValidationError};
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

        for file in files {
            let source = if place.flatten {
                file.source.file_name()
            } else {
                file.source.as_str()
            };
            let path = RelPath::new(&format!("{}/{}", target.path, source))?;
            operations.push(Operation::Materialize {
                path,
                blob: file.blob,
                mutable: file.mutable,
            });
        }
    }
    Ok(OperationSet { operations })
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
    use msbe_plan_schema::{Loader, NamedPath, PlaceStep, Plan, SCHEMA_VERSION, Side, Step};

    use super::{ResolveError, ResolvedFile, resolve};

    fn plan(flatten: bool) -> Plan {
        Plan {
            schema: SCHEMA_VERSION,
            id: "example".to_owned(),
            name: "Example".to_owned(),
            version: "1.0.0".to_owned(),
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
    fn rejects_a_loader_that_the_plan_does_not_declare() {
        let error = resolve(&plan(true), "other", &[]).unwrap_err();
        assert!(matches!(error, ResolveError::UnknownLoader(loader) if loader == "other"));
    }
}
