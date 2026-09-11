//! Deterministic candidate-halving for diagnosing a broken profile.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::instance::Name;

/// A resumable search for one broken module in a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    /// The profile whose modules are being diagnosed.
    pub profile: Name,
    /// The isolated profile containing the current trial subset.
    pub trial_profile: Name,
    /// Modules that can still be responsible.
    pub candidates: BTreeSet<Name>,
    /// The subset currently deployed for a user verdict.
    pub trial: BTreeSet<Name>,
}

impl Session {
    /// Starts a deterministic bisection over `candidates`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TooFewCandidates`] when there is no meaningful bisection to run.
    pub fn new(
        profile: Name,
        trial_profile: Name,
        candidates: BTreeSet<Name>,
    ) -> Result<Self, Error> {
        if candidates.len() < 2 {
            return Err(Error::TooFewCandidates);
        }
        let trial = half(&candidates);
        Ok(Self {
            profile,
            trial_profile,
            candidates,
            trial,
        })
    }

    /// Records whether the currently deployed trial reproduces the failure.
    ///
    /// A failing trial keeps that half; a working trial keeps its complement. Returns the
    /// culprit once exactly one candidate remains.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidTrial`] if persisted state has no trial subset.
    pub fn record(&mut self, failing: bool) -> Result<Option<&Name>, Error> {
        if self.trial.is_empty() {
            return Err(Error::InvalidTrial);
        }
        if failing {
            self.candidates.clone_from(&self.trial);
        } else {
            self.candidates = self.candidates.difference(&self.trial).cloned().collect();
        }
        if self.candidates.len() == 1 {
            self.trial.clear();
            return Ok(self.candidates.first());
        }
        self.trial = half(&self.candidates);
        Ok(None)
    }
}

fn half(candidates: &BTreeSet<Name>) -> BTreeSet<Name> {
    candidates
        .iter()
        .take(candidates.len().div_ceil(2))
        .cloned()
        .collect()
}

/// Bisection state is invalid or cannot narrow candidates.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A bisection needs at least two candidate modules.
    #[error("bisection needs at least two candidate modules")]
    TooFewCandidates,
    /// The persisted session did not retain a trial subset.
    #[error("bisection session has no trial subset")]
    InvalidTrial,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::Session;
    use crate::instance::Name;

    fn names(values: &[&str]) -> BTreeSet<Name> {
        values.iter().map(|name| Name::new(name).unwrap()).collect()
    }

    #[test]
    fn narrows_to_the_failing_half_and_then_the_culprit() {
        let mut session = Session::new(
            Name::new("default").unwrap(),
            Name::new("msbe-bisect").unwrap(),
            names(&["a", "b", "c"]),
        )
        .unwrap();
        assert_eq!(session.trial, names(&["a", "b"]));
        assert!(session.record(true).unwrap().is_none());
        assert_eq!(session.trial, names(&["a"]));
        assert_eq!(
            session.record(false).unwrap(),
            Some(&Name::new("b").unwrap())
        );
    }
}
