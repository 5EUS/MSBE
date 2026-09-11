//! Provider-neutral PubGrub dependency resolution.
//!
//! Providers supply target-filtered [`Candidate`] records. The solver knows neither provider
//! protocols nor game formats: it resolves exact provider release IDs and unconstrained package
//! requirements, returning the selected candidates or PubGrub's derivation explanation.
//!
//! # Virtual packages
//!
//! A release may declare that it `provides` or `replaces` other packages (`docs/05-solver.md`
//! §5.5): an API reimplementation provides the API it reimplements, and a maintained successor
//! replaces an abandoned original. A requirement without an exact release is then met by the
//! required package itself or by any selected release that supplies it:
//!
//! - **At most one selected package supplies a package**, the package itself included. Two
//!   implementations of one API claim the same identity and cannot load together.
//! - **Preference**, when nothing else decides: a replacement over the original it replaces, and
//!   the original over a package that merely provides it.
//! - **An exact release names that release**, so only the package itself can meet it.
//! - **Roots are what the user asked for by name**, so a substitute never meets one.
//!
//! Each supplied package becomes a PubGrub package of its own whose versions are its suppliers,
//! the most preferred highest. A supplier version depends on the releases that supply, and every
//! supplying release depends on its own supplier version, which is what makes suppliers exclusive.

use std::{collections::BTreeMap, fmt, iter};

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
    /// An exact provider release ID, or `None` for any compatible candidate, including one from a
    /// package that provides or replaces the required package.
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
    /// Packages this release can stand in for, such as an API it reimplements.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provides: Vec<PackageId>,
    /// Packages this release succeeds, and is preferred over when either would do.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replaces: Vec<PackageId>,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Solution {
    /// Selected releases indexed by provider-stable package identity.
    pub selected: BTreeMap<PackageId, Candidate>,
    /// Packages that a different selected package provides or replaces, mapped to that package.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub supplied_by: BTreeMap<PackageId, PackageId>,
}

/// Resolves `roots` against the complete target-filtered `candidates` set.
///
/// Every candidate referenced by a root or dependency must be present in `candidates`; an absent
/// exact release becomes an explainable PubGrub conflict. The caller must remove target-ineligible
/// releases before calling this function. A package that provides or replaces a required package
/// can only be chosen if its releases are among `candidates` too.
///
/// # Errors
///
/// Returns [`SolverError::NoSolution`] with PubGrub's explanation when requirements conflict.
pub fn solve(roots: &[RootRequirement], candidates: &[Candidate]) -> Result<Solution, SolverError> {
    let index = Index::new(candidates)?;
    let suppliers = suppliers(candidates)?;
    let slots = slots(&suppliers);

    let mut provider = OfflineDependencyProvider::<Node, Versions>::new();
    provider.add_dependencies(
        Node::Root,
        1_u64,
        constraints(roots.iter().map(|root| {
            let range = index.release_range(&root.package, root.release.as_deref());
            (Node::Package(root.package.clone()), range)
        })),
    );
    for (supplied, ranked) in &slots {
        for (supplier, rank) in ranked {
            let releases = suppliers
                .get(supplied)
                .and_then(|by| by.get(supplier))
                .map_or_else(Ranges::full, |(_, releases)| releases.clone());
            provider.add_dependencies(
                Node::Supplier(supplied.clone()),
                *rank,
                [(Node::Package(supplier.clone()), releases)],
            );
        }
    }
    for candidate in candidates {
        let dependencies = candidate
            .dependencies
            .iter()
            .map(|requirement| index.requirement_constraint(requirement, &slots));
        provider.add_dependencies(
            Node::Package(candidate.package.clone()),
            candidate.order,
            constraints(dependencies.chain(claims(candidate, &slots))),
        );
    }

    let selected = match resolve(&provider, Node::Root, 1_u64) {
        Ok(selected) => selected,
        Err(PubGrubError::NoSolution(mut tree)) => {
            tree.collapse_no_versions();
            return Err(SolverError::NoSolution(DefaultStringReporter::report(
                &tree,
            )));
        }
        Err(error) => return Err(SolverError::Engine(error.to_string())),
    };
    let mut solution = Solution::default();
    for (node, rank) in selected {
        match node {
            Node::Root => {}
            Node::Package(package) => {
                let candidate = index
                    .by_rank
                    .get(&(package.clone(), rank))
                    .cloned()
                    .ok_or(SolverError::MissingSelected { package, rank })?;
                solution
                    .selected
                    .insert(candidate.package.clone(), candidate);
            }
            Node::Supplier(supplied) => {
                let chosen = slots
                    .get(&supplied)
                    .and_then(|ranked| ranked.iter().find(|(_, slot)| **slot == rank))
                    .map(|(package, _)| package.clone())
                    .ok_or_else(|| SolverError::MissingSelected {
                        package: supplied.clone(),
                        rank,
                    })?;
                if chosen != supplied {
                    solution.supplied_by.insert(supplied, chosen);
                }
            }
        }
    }
    Ok(solution)
}

/// A package as PubGrub sees it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Node {
    /// The user's selections.
    Root,
    /// A real package, whose versions are its candidates' ranks.
    Package(PackageId),
    /// Which package supplies this one, whose versions are the ranks in [`Slots`].
    Supplier(PackageId),
}

impl fmt::Display for Node {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root => formatter.write_str("msbe:root"),
            Self::Package(package) => package.fmt(formatter),
            Self::Supplier(package) => write!(formatter, "a supplier of {package}"),
        }
    }
}

/// How one package supplies another, least preferred over the original first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Supply {
    Provides,
    Replaces,
}

/// For each supplied package, the other packages that supply it, how, and which of their
/// releases do.
type Suppliers = BTreeMap<PackageId, BTreeMap<PackageId, (Supply, Versions)>>;

/// For each supplied package, every package that can supply it, the package itself included,
/// ranked with the most preferred highest.
type Slots = BTreeMap<PackageId, BTreeMap<PackageId, u64>>;

/// The candidates' ranks by package and release, and back again.
struct Index {
    ranks: BTreeMap<PackageId, BTreeMap<String, u64>>,
    by_rank: BTreeMap<(PackageId, u64), Candidate>,
}

impl Index {
    fn new(candidates: &[Candidate]) -> Result<Self, SolverError> {
        let mut index = Self {
            ranks: BTreeMap::new(),
            by_rank: BTreeMap::new(),
        };
        for candidate in candidates {
            if candidate.release.is_empty() {
                return Err(SolverError::EmptyRelease(candidate.package.clone()));
            }
            if index
                .ranks
                .entry(candidate.package.clone())
                .or_default()
                .insert(candidate.release.clone(), candidate.order)
                .is_some()
                || index
                    .by_rank
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
        Ok(index)
    }

    /// An exact release requirement names one package; an open one accepts any supplier.
    fn requirement_constraint(&self, requirement: &Requirement, slots: &Slots) -> (Node, Versions) {
        match requirement.release.as_deref() {
            None if slots.contains_key(&requirement.package) => {
                (Node::Supplier(requirement.package.clone()), Ranges::full())
            }
            release => (
                Node::Package(requirement.package.clone()),
                self.release_range(&requirement.package, release),
            ),
        }
    }

    fn release_range(&self, package: &PackageId, release: Option<&str>) -> Versions {
        release.map_or_else(Ranges::full, |release| {
            self.ranks
                .get(package)
                .and_then(|releases| releases.get(release))
                .map_or_else(Ranges::empty, |rank| Ranges::singleton(*rank))
        })
    }
}

fn suppliers(candidates: &[Candidate]) -> Result<Suppliers, SolverError> {
    let mut suppliers = Suppliers::new();
    for candidate in candidates {
        let declared = (candidate
            .provides
            .iter()
            .map(|package| (package, Supply::Provides)))
        .chain(
            candidate
                .replaces
                .iter()
                .map(|package| (package, Supply::Replaces)),
        );
        for (supplied, supply) in declared {
            if *supplied == candidate.package {
                return Err(SolverError::SuppliesItself(candidate.package.clone()));
            }
            let (strongest, releases) = suppliers
                .entry(supplied.clone())
                .or_default()
                .entry(candidate.package.clone())
                .or_insert((supply, Ranges::empty()));
            *strongest = (*strongest).max(supply);
            *releases = releases.union(&Ranges::singleton(candidate.order));
        }
    }
    Ok(suppliers)
}

fn slots(suppliers: &Suppliers) -> Slots {
    suppliers
        .iter()
        .map(|(supplied, by)| {
            let supplying = |wanted: Supply| {
                by.iter()
                    .filter(move |(_, (supply, _))| *supply == wanted)
                    .map(|(package, _)| package)
            };
            let preferred: Vec<&PackageId> = supplying(Supply::Replaces)
                .chain(iter::once(supplied))
                .chain(supplying(Supply::Provides))
                .collect();
            let ranked = preferred
                .into_iter()
                .rev()
                .zip(1_u64..)
                .map(|(package, rank)| (package.clone(), rank))
                .collect();
            (supplied.clone(), ranked)
        })
        .collect()
}

/// The supplier slots `candidate` occupies: its own package's, and those of every package it
/// provides or replaces.
fn claims<'a>(
    candidate: &'a Candidate,
    slots: &'a Slots,
) -> impl Iterator<Item = (Node, Versions)> + 'a {
    iter::once(&candidate.package)
        .chain(&candidate.provides)
        .chain(&candidate.replaces)
        .filter_map(|supplied| {
            let rank = slots.get(supplied)?.get(&candidate.package)?;
            Some((Node::Supplier(supplied.clone()), Ranges::singleton(*rank)))
        })
}

/// Collects constraints, intersecting any that name the same node rather than keeping only the
/// last.
fn constraints(
    requirements: impl IntoIterator<Item = (Node, Versions)>,
) -> DependencyConstraints<Node, Versions> {
    let mut merged = DependencyConstraints::default();
    for (node, range) in requirements {
        let range = match merged.remove(&node) {
            Some(existing) => range.intersection(&existing),
            None => range,
        };
        merged.insert(node, range);
    }
    merged
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
    /// A candidate declares that it provides or replaces its own package.
    #[error("candidate for {0} provides or replaces itself")]
    SuppliesItself(PackageId),
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
            provides: Vec::new(),
            replaces: Vec::new(),
        }
    }

    fn requires(name: &str) -> Vec<Requirement> {
        vec![Requirement {
            package: package(name),
            release: None,
        }]
    }

    fn providing(mut candidate: Candidate, name: &str) -> Candidate {
        candidate.provides.push(package(name));
        candidate
    }

    fn replacing(mut candidate: Candidate, name: &str) -> Candidate {
        candidate.replaces.push(package(name));
        candidate
    }

    fn roots(names: &[&str]) -> Vec<RootRequirement> {
        names
            .iter()
            .map(|name| RootRequirement {
                package: package(name),
                release: None,
            })
            .collect()
    }

    fn selected(solution: &super::Solution) -> Vec<&str> {
        solution
            .selected
            .keys()
            .map(|package| package.project.as_str())
            .collect()
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
        let solution = solve(&roots(&["a", "c"]), &candidates).unwrap();
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
        let error = solve(&roots(&["a", "c"]), &candidates).unwrap_err();
        assert!(matches!(error, SolverError::NoSolution(_)));
    }

    #[test]
    fn a_selected_provider_meets_a_requirement_without_adding_the_original() {
        let candidates = [
            candidate("mod", "m1", 1, requires("api")),
            candidate("api", "api1", 1, Vec::new()),
            providing(candidate("fork", "f1", 1, Vec::new()), "api"),
        ];
        let solution = solve(&roots(&["fork", "mod"]), &candidates).unwrap();
        assert_eq!(selected(&solution), ["fork", "mod"]);
        assert_eq!(
            solution.supplied_by.get(&package("api")),
            Some(&package("fork"))
        );
    }

    #[test]
    fn the_original_is_preferred_over_a_provider_when_nothing_decides() {
        let candidates = [
            candidate("mod", "m1", 1, requires("api")),
            candidate("api", "api1", 1, Vec::new()),
            providing(candidate("fork", "f1", 1, Vec::new()), "api"),
        ];
        let solution = solve(&roots(&["mod"]), &candidates).unwrap();
        assert_eq!(selected(&solution), ["api", "mod"]);
        assert!(solution.supplied_by.is_empty());
    }

    #[test]
    fn a_provider_stands_in_when_the_original_has_no_candidates() {
        let candidates = [
            candidate("mod", "m1", 1, requires("api")),
            providing(candidate("fork", "f1", 1, Vec::new()), "api"),
        ];
        let solution = solve(&roots(&["mod"]), &candidates).unwrap();
        assert_eq!(selected(&solution), ["fork", "mod"]);
    }

    #[test]
    fn a_replacement_is_preferred_over_the_original_it_replaces() {
        let candidates = [
            candidate("mod", "m1", 1, requires("abandoned")),
            candidate("abandoned", "a1", 1, Vec::new()),
            providing(candidate("fork", "f1", 1, Vec::new()), "abandoned"),
            replacing(candidate("successor", "s1", 1, Vec::new()), "abandoned"),
        ];
        let solution = solve(&roots(&["mod"]), &candidates).unwrap();
        assert_eq!(selected(&solution), ["mod", "successor"]);
        assert_eq!(
            solution.supplied_by.get(&package("abandoned")),
            Some(&package("successor"))
        );
    }

    #[test]
    fn a_root_is_never_met_by_a_substitute() {
        let candidates = [
            candidate("abandoned", "a1", 1, Vec::new()),
            replacing(candidate("successor", "s1", 1, Vec::new()), "abandoned"),
        ];
        let solution = solve(&roots(&["abandoned"]), &candidates).unwrap();
        assert_eq!(selected(&solution), ["abandoned"]);
    }

    #[test]
    fn two_suppliers_of_one_package_cannot_both_be_selected() {
        let candidates = [
            candidate("api", "api1", 1, Vec::new()),
            providing(candidate("fork", "f1", 1, Vec::new()), "api"),
            providing(candidate("other", "o1", 1, Vec::new()), "api"),
        ];
        for pair in [["api", "fork"], ["fork", "other"]] {
            let error = solve(&roots(&pair), &candidates).unwrap_err();
            let SolverError::NoSolution(explanation) = error else {
                panic!("expected an explained conflict for {pair:?}, got {error:?}");
            };
            assert!(
                explanation.contains("a supplier of test:api"),
                "{explanation}"
            );
        }
    }

    #[test]
    fn an_exact_release_requirement_is_only_met_by_the_package_itself() {
        let exact = vec![Requirement {
            package: package("api"),
            release: Some("api1".to_owned()),
        }];
        let candidates = [
            candidate("mod", "m1", 1, exact),
            candidate("api", "api1", 1, Vec::new()),
            providing(candidate("fork", "f1", 1, Vec::new()), "api"),
        ];
        assert_eq!(
            selected(&solve(&roots(&["mod"]), &candidates).unwrap()),
            ["api", "mod"]
        );
        assert!(matches!(
            solve(&roots(&["mod", "fork"]), &candidates),
            Err(SolverError::NoSolution(_))
        ));
    }

    #[test]
    fn only_releases_that_declare_a_supply_stand_in() {
        let candidates = [
            candidate("mod", "m1", 1, requires("api")),
            candidate("api", "api1", 1, Vec::new()),
            candidate("fork", "f1", 1, Vec::new()),
            providing(candidate("fork", "f2", 2, Vec::new()), "api"),
        ];
        let old_fork = [
            RootRequirement {
                package: package("fork"),
                release: Some("f1".to_owned()),
            },
            RootRequirement {
                package: package("mod"),
                release: None,
            },
        ];
        let solution = solve(&old_fork, &candidates).unwrap();
        assert_eq!(selected(&solution), ["api", "fork", "mod"]);

        let solution = solve(&roots(&["fork", "mod"]), &candidates).unwrap();
        assert_eq!(selected(&solution), ["fork", "mod"]);
        assert_eq!(
            solution.selected.get(&package("fork")).unwrap().release,
            "f2"
        );
    }

    #[test]
    fn a_candidate_that_supplies_itself_is_rejected() {
        let candidates = [providing(candidate("api", "api1", 1, Vec::new()), "api")];
        assert!(matches!(
            solve(&roots(&["api"]), &candidates),
            Err(SolverError::SuppliesItself(package)) if package.project == "api"
        ));
    }
}
