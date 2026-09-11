//! Provider-neutral PubGrub dependency resolution.
//!
//! Providers supply target-filtered [`Candidate`] records. The solver knows neither provider
//! protocols nor game formats: it resolves exact provider release IDs and unconstrained package
//! requirements, returning the selected candidates or PubGrub's derivation explanation.

use std::{collections::BTreeMap, fmt};

use pubgrub::{
    DefaultStringReporter, DependencyConstraints, OfflineDependencyProvider, PubGrubError, Ranges,
    Reporter, resolve,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

type Versions = Ranges<u64>;

/// A package identity that is stable across providers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PackageId {
    /// The reviewed provider's stable identifier.
    pub provider: String,
    /// The provider's stable project identifier.
    pub project: String,
}

impl fmt::Display for PackageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.provider, self.project)
    }
}

/// A requirement on another package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Requirement {
    /// The required package.
    pub package: PackageId,
    /// An exact provider release ID, or `None` for any compatible candidate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
}

/// One target-filtered provider release available to the solver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    /// The package this release belongs to.
    pub package: PackageId,
    /// The provider's stable release ID.
    pub release: String,
    /// Provider ordering where a larger value is preferred for an unconstrained requirement.
    pub order: u64,
    /// Hard requirements declared by this release.
    #[serde(default)]
    pub dependencies: Vec<Requirement>,
}

/// A direct user selection to resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootRequirement {
    /// The requested package.
    pub package: PackageId,
    /// An exact release selected or pinned by the user, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
}

/// A complete, deterministic PubGrub solution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Solution {
    /// Selected releases indexed by provider-stable package identity.
    pub selected: BTreeMap<PackageId, Candidate>,
}

/// Resolves `roots` against the complete target-filtered `candidates` set.
///
/// Every candidate referenced by a root or dependency must be present in `candidates`; an absent
/// exact release becomes an explainable PubGrub conflict. The caller must remove target-ineligible
/// releases before calling this function.
///
/// # Errors
///
/// Returns [`SolverError::NoSolution`] with PubGrub's explanation when requirements conflict.
pub fn solve(roots: &[RootRequirement], candidates: &[Candidate]) -> Result<Solution, SolverError> {
    let root = PackageId {
        provider: "msbe".to_owned(),
        project: "root".to_owned(),
    };
    let mut ranks: BTreeMap<PackageId, BTreeMap<String, u64>> = BTreeMap::new();
    let mut by_rank: BTreeMap<(PackageId, u64), Candidate> = BTreeMap::new();
    for candidate in candidates {
        if candidate.release.is_empty() {
            return Err(SolverError::EmptyRelease(candidate.package.clone()));
        }
        if ranks
            .entry(candidate.package.clone())
            .or_default()
            .insert(candidate.release.clone(), candidate.order)
            .is_some()
            || by_rank
                .insert(
                    (candidate.package.clone(), candidate.order),
                    candidate.clone(),
                )
                .is_some()
        {
            return Err(SolverError::DuplicateCandidate {
                package: candidate.package.clone(),
                release: candidate.release.clone(),
            });
        }
    }

    let mut provider = OfflineDependencyProvider::<PackageId, Versions>::new();
    let root_dependencies = constraints(roots.iter().map(|requirement| {
        let range = release_range(&requirement.package, requirement.release.as_deref(), &ranks);
        (requirement.package.clone(), range)
    }));
    provider.add_dependencies(root.clone(), 1_u64, root_dependencies);
    for candidate in candidates {
        let dependencies = constraints(candidate.dependencies.iter().map(|requirement| {
            let range = requirement_range(requirement, &ranks);
            (requirement.package.clone(), range)
        }));
        provider.add_dependencies(candidate.package.clone(), candidate.order, dependencies);
    }

    let selected = match resolve(&provider, root.clone(), 1_u64) {
        Ok(selected) => selected,
        Err(PubGrubError::NoSolution(mut tree)) => {
            tree.collapse_no_versions();
            return Err(SolverError::NoSolution(DefaultStringReporter::report(
                &tree,
            )));
        }
        Err(error) => return Err(SolverError::Engine(error.to_string())),
    };
    let mut solution = BTreeMap::new();
    for (package, rank) in selected {
        if package == root {
            continue;
        }
        let candidate = by_rank
            .get(&(package.clone(), rank))
            .cloned()
            .ok_or(SolverError::MissingSelected { package, rank })?;
        solution.insert(candidate.package.clone(), candidate);
    }
    Ok(Solution { selected: solution })
}

fn constraints(
    requirements: impl IntoIterator<Item = (PackageId, Ranges<u64>)>,
) -> DependencyConstraints<PackageId, Ranges<u64>> {
    requirements.into_iter().collect()
}

fn requirement_range(
    requirement: &Requirement,
    ranks: &BTreeMap<PackageId, BTreeMap<String, u64>>,
) -> Ranges<u64> {
    release_range(&requirement.package, requirement.release.as_deref(), ranks)
}

fn release_range(
    package: &PackageId,
    release: Option<&str>,
    ranks: &BTreeMap<PackageId, BTreeMap<String, u64>>,
) -> Ranges<u64> {
    release.map_or_else(Ranges::full, |release| {
        ranks
            .get(package)
            .and_then(|releases| releases.get(release))
            .map_or_else(Ranges::empty, |rank| Ranges::singleton(*rank))
    })
}

/// Why resolving target-filtered provider releases failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SolverError {
    /// A candidate did not provide a stable release ID.
    #[error("candidate for {0} has an empty release id")]
    EmptyRelease(PackageId),
    /// Two records describe the same release or priority slot.
    #[error("duplicate candidate {package} release {release:?}")]
    DuplicateCandidate {
        /// The duplicated package.
        package: PackageId,
        /// The duplicated release ID.
        release: String,
    },
    /// PubGrub selected a candidate not present in the normalized input.
    #[error("solver selected unknown candidate {package} rank {rank}")]
    MissingSelected {
        /// The selected package.
        package: PackageId,
        /// The provider ordering rank.
        rank: u64,
    },
    /// PubGrub determined that requirements cannot all be satisfied.
    #[error("dependency resolution failed:\n{0}")]
    NoSolution(String),
    /// An internal PubGrub operation failed unexpectedly.
    #[error("dependency solver failed: {0}")]
    Engine(String),
}

#[cfg(test)]
mod tests {
    use super::{Candidate, PackageId, Requirement, RootRequirement, SolverError, solve};

    fn package(name: &str) -> PackageId {
        PackageId {
            provider: "test".to_owned(),
            project: name.to_owned(),
        }
    }

    fn candidate(
        name: &str,
        release: &str,
        order: u64,
        dependencies: Vec<Requirement>,
    ) -> Candidate {
        Candidate {
            package: package(name),
            release: release.to_owned(),
            order,
            dependencies,
        }
    }

    #[test]
    fn chooses_newest_release_that_satisfies_transitive_exact_requirement() {
        let candidates = [
            candidate(
                "a",
                "a1",
                1,
                vec![Requirement {
                    package: package("b"),
                    release: Some("b1".to_owned()),
                }],
            ),
            candidate(
                "a",
                "a2",
                2,
                vec![Requirement {
                    package: package("b"),
                    release: Some("b2".to_owned()),
                }],
            ),
            candidate("b", "b1", 1, Vec::new()),
            candidate("b", "b2", 2, Vec::new()),
            candidate(
                "c",
                "c1",
                1,
                vec![Requirement {
                    package: package("b"),
                    release: Some("b1".to_owned()),
                }],
            ),
        ];
        let roots = [
            RootRequirement {
                package: package("a"),
                release: None,
            },
            RootRequirement {
                package: package("c"),
                release: None,
            },
        ];
        let solution = solve(&roots, &candidates).unwrap();
        assert_eq!(solution.selected.get(&package("a")).unwrap().release, "a1");
        assert_eq!(solution.selected.get(&package("b")).unwrap().release, "b1");
    }

    #[test]
    fn explains_unsatisfiable_exact_requirements() {
        let candidates = [
            candidate(
                "a",
                "a1",
                1,
                vec![Requirement {
                    package: package("b"),
                    release: Some("b1".to_owned()),
                }],
            ),
            candidate(
                "c",
                "c1",
                1,
                vec![Requirement {
                    package: package("b"),
                    release: Some("b2".to_owned()),
                }],
            ),
            candidate("b", "b1", 1, Vec::new()),
            candidate("b", "b2", 2, Vec::new()),
        ];
        let roots = [
            RootRequirement {
                package: package("a"),
                release: None,
            },
            RootRequirement {
                package: package("c"),
                release: None,
            },
        ];
        let error = solve(&roots, &candidates).unwrap_err();
        assert!(matches!(error, SolverError::NoSolution(_)));
    }
}
