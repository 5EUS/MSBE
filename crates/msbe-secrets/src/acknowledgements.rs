//! The provider terms a user acknowledged.

use std::path::PathBuf;

use msbe_core::config::Home;
use serde::{Deserialize, Serialize};

use crate::{Clock, StoreError, files, store::validate_provider};

const FILE: &str = "acknowledgements.toml";
const KIND: &str = "acknowledgements file";

/// One provider's acknowledged terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acknowledgement {
    /// The provider id.
    pub provider: String,
    /// The terms URL the provider declared when they were acknowledged; empty when it declared
    /// none.
    pub terms: String,
    /// The canonical digest of the provider program they were acknowledged under. A native
    /// provider has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// When, as Unix seconds.
    pub acknowledged: u64,
}

impl Acknowledgement {
    /// Whether this acknowledges `provider`'s current `terms` under its current `program`. Changed
    /// terms, or a changed program, need a new acknowledgement.
    pub fn covers(&self, provider: &str, terms: &str, program: Option<&str>) -> bool {
        self.provider == provider && self.terms == terms && self.program.as_deref() == program
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    #[serde(default, rename = "acknowledgement")]
    entries: Vec<Acknowledgement>,
}

#[derive(Serialize)]
struct DocumentRef<'a> {
    #[serde(rename = "acknowledgement")]
    entries: &'a [Acknowledgement],
}

/// A data directory's `acknowledgements.toml`.
#[derive(Debug)]
pub struct Acknowledgements {
    path: PathBuf,
    entries: Vec<Acknowledgement>,
}

impl Acknowledgements {
    /// Reads `home`'s acknowledgements. A missing file has none.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Malformed`] or [`StoreError::Io`] when the file cannot be read.
    pub fn load(home: &Home) -> Result<Self, StoreError> {
        let path = files::auth_directory(home).join(FILE);
        let entries = match files::read_text(&path, KIND)? {
            Some(text) => {
                toml::from_str::<Document>(&text)
                    .map_err(|error| files::malformed(&path, KIND, error.to_string()))?
                    .entries
            }
            None => Vec::new(),
        };
        Ok(Self { path, entries })
    }

    /// Every acknowledgement, ordered by provider.
    pub fn entries(&self) -> &[Acknowledgement] {
        &self.entries
    }

    /// Every acknowledgement, ordered by provider.
    pub fn into_entries(self) -> Vec<Acknowledgement> {
        self.entries
    }

    /// Whether `provider`'s current `terms` under its current `program` are acknowledged.
    pub fn covers(&self, provider: &str, terms: &str, program: Option<&str>) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.covers(provider, terms, program))
    }

    /// Records that `provider`'s `terms` were acknowledged under `program`, replacing its earlier
    /// acknowledgement.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for an invalid provider id or a file that cannot be written.
    pub fn acknowledge(
        &mut self,
        provider: &str,
        terms: &str,
        program: Option<&str>,
        clock: &dyn Clock,
    ) -> Result<(), StoreError> {
        validate_provider(provider)?;
        self.entries.retain(|entry| entry.provider != provider);
        self.entries.push(Acknowledgement {
            provider: provider.to_owned(),
            terms: terms.to_owned(),
            program: program.map(str::to_owned),
            acknowledged: clock.now(),
        });
        self.entries
            .sort_by(|left, right| left.provider.cmp(&right.provider));
        self.save()
    }

    /// Withdraws `provider`'s acknowledgement, reporting whether it had one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the file cannot be written.
    pub fn withdraw(&mut self, provider: &str) -> Result<bool, StoreError> {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.provider != provider);
        if self.entries.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    fn save(&self) -> Result<(), StoreError> {
        let text = toml::to_string(&DocumentRef {
            entries: &self.entries,
        })
        .map_err(|error| files::malformed(&self.path, KIND, error.to_string()))?;
        files::write_private(&self.path, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use msbe_core::config::Home;

    use super::Acknowledgements;
    use crate::Clock;

    #[derive(Debug)]
    struct Fixed(u64);

    impl Clock for Fixed {
        fn now(&self) -> u64 {
            self.0
        }
    }

    #[test]
    fn acknowledgements_are_keyed_by_terms_and_program() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        let mut acknowledgements = Acknowledgements::load(&home).unwrap();
        assert!(!acknowledgements.covers("example", "https://example.test/terms", None));
        acknowledgements
            .acknowledge(
                "example",
                "https://example.test/terms",
                Some("abc123"),
                &Fixed(7),
            )
            .unwrap();

        let reloaded = Acknowledgements::load(&home).unwrap();
        assert!(reloaded.covers("example", "https://example.test/terms", Some("abc123")));
        assert!(!reloaded.covers("example", "https://example.test/terms-v2", Some("abc123")));
        assert!(!reloaded.covers("example", "https://example.test/terms", Some("def456")));
        assert_eq!(reloaded.entries().first().unwrap().acknowledged, 7);

        let mut reloaded = reloaded;
        assert!(reloaded.withdraw("example").unwrap());
        assert!(!reloaded.withdraw("example").unwrap());
        assert!(Acknowledgements::load(&home).unwrap().entries().is_empty());
    }
}
