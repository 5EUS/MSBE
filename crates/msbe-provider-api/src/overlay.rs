//! Curated relationships between mods that providers do not publish.
//!
//! Upstreams publish that a mod exists; they rarely publish that a fork reimplements another
//! mod's API, or that a successor replaces an abandoned mod. An overlay entry records that for
//! one mod so the solver can act on it (`docs/10-registry.md`, `docs/05-solver.md` §5.5). The
//! entries MSBE ships are compiled in; the registry will distribute signed ones in the same format.

use std::collections::BTreeMap;

use msbe_core::solver::PackageId;
use serde::Deserialize;
use thiserror::Error;

/// The only overlay schema version understood by this release.
pub const SCHEMA_VERSION: u32 = 1;

/// Validated overlay entries, keyed by the mod each describes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overlay {
    entries: BTreeMap<PackageId, Entry>,
}

/// What the overlay records about one mod.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// Packages the mod can stand in for, such as an API it reimplements.
    pub provides: Vec<PackageId>,
    /// Packages the mod succeeds, and is preferred over when either would do.
    pub replaces: Vec<PackageId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema: u32,
    #[serde(rename = "mod")]
    module: String,
    #[serde(default)]
    provides: Vec<String>,
    #[serde(default)]
    replaces: Vec<String>,
}

impl Overlay {
    /// Parses and validates an overlay from independent TOML documents, one per mod.
    ///
    /// # Errors
    ///
    /// Returns [`OverlayError`] if a document is malformed, contradicts itself, or describes a
    /// mod another document already describes.
    pub fn from_toml(documents: &[&str]) -> Result<Self, OverlayError> {
        let mut entries = BTreeMap::new();
        for document in documents {
            let document: Document =
                toml::from_str(document).map_err(|error| OverlayError::Parse(error.to_string()))?;
            if document.schema != SCHEMA_VERSION {
                return Err(OverlayError::UnsupportedSchema(document.schema));
            }
            let module = reference(&document.module)?;
            let entry = Entry {
                provides: references(&document.provides)?,
                replaces: references(&document.replaces)?,
            };
            if entry
                .provides
                .iter()
                .chain(&entry.replaces)
                .any(|package| *package == module)
            {
                return Err(OverlayError::SuppliesItself(document.module));
            }
            if let Some(package) = entry
                .provides
                .iter()
                .find(|package| entry.replaces.contains(package))
            {
                return Err(OverlayError::ProvidesAndReplaces {
                    module: document.module,
                    package: package.to_string(),
                });
            }
            if entries.contains_key(&module) {
                return Err(OverlayError::DuplicateEntry(document.module));
            }
            entries.insert(module, entry);
        }
        Ok(Self { entries })
    }

    /// What the overlay records about `package`, if anything.
    pub fn entry(&self, package: &PackageId) -> Option<&Entry> {
        self.entries.get(package)
    }

    /// Every mod that provides or replaces `package`.
    pub fn suppliers<'a>(
        &'a self,
        package: &PackageId,
    ) -> impl Iterator<Item = &'a PackageId> + use<'a> {
        let package = package.clone();
        self.entries
            .iter()
            .filter(move |(_, entry)| {
                entry.provides.contains(&package) || entry.replaces.contains(&package)
            })
            .map(|(module, _)| module)
    }
}

fn references(raw: &[String]) -> Result<Vec<PackageId>, OverlayError> {
    raw.iter()
        .map(|reference| self::reference(reference))
        .collect()
}

/// Parses `provider:project`, where neither part is empty or contains whitespace.
fn reference(raw: &str) -> Result<PackageId, OverlayError> {
    let is_part = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|character| character.is_ascii_graphic() && character != ':')
    };
    raw.split_once(':')
        .filter(|(provider, project)| is_part(provider) && is_part(project))
        .map(|(provider, project)| PackageId {
            provider: provider.to_owned(),
            project: project.to_owned(),
        })
        .ok_or_else(|| OverlayError::InvalidReference(raw.to_owned()))
}

/// Why an overlay document was rejected.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OverlayError {
    /// A document is not valid TOML or does not match the overlay schema.
    #[error("invalid overlay entry: {0}")]
    Parse(String),
    /// A document declares a schema version this release does not understand.
    #[error("overlay schema {0} is not supported")]
    UnsupportedSchema(u32),
    /// A mod reference is not `provider:project`.
    #[error("{0:?} is not a provider:project reference")]
    InvalidReference(String),
    /// A mod is declared to provide or replace itself.
    #[error("overlay entry for {0} provides or replaces itself")]
    SuppliesItself(String),
    /// A mod both provides and replaces the same package, which gives no single preference.
    #[error("overlay entry for {module} both provides and replaces {package}")]
    ProvidesAndReplaces {
        /// The described mod.
        module: String,
        /// The package named twice.
        package: String,
    },
    /// Two documents describe the same mod.
    #[error("more than one overlay entry describes {0}")]
    DuplicateEntry(String),
}

#[cfg(test)]
mod tests {
    use msbe_core::solver::PackageId;

    use super::{Overlay, OverlayError};

    fn provider(project: &str) -> PackageId {
        PackageId {
            provider: "example".to_owned(),
            project: project.to_owned(),
        }
    }

    #[test]
    fn entries_record_provides_and_replaces() {
        let overlay = Overlay::from_toml(&[r#"
            schema = 1
            mod = "example:successor"
            provides = ["example:api"]
            replaces = ["example:abandoned"]
        "#])
        .unwrap();
        let entry = overlay.entry(&provider("successor")).unwrap();
        assert_eq!(entry.provides, [provider("api")]);
        assert_eq!(entry.replaces, [provider("abandoned")]);
        assert!(overlay.entry(&provider("api")).is_none());
    }

    #[test]
    fn invalid_or_contradictory_entries_are_rejected() {
        let rejection = |document: &str| Overlay::from_toml(&[document]).unwrap_err();
        assert!(matches!(
            rejection("schema = 2\nmod = \"example:a\""),
            OverlayError::UnsupportedSchema(2)
        ));
        assert!(matches!(
            rejection("schema = 1\nmod = \"example:a\"\nconflicts = []"),
            OverlayError::Parse(_)
        ));
        assert!(matches!(
            rejection("schema = 1\nmod = \"a\""),
            OverlayError::InvalidReference(_)
        ));
        assert!(matches!(
            rejection("schema = 1\nmod = \"example:a\"\nreplaces = [\"example:a\"]"),
            OverlayError::SuppliesItself(_)
        ));
        assert!(matches!(
            rejection(
                "schema = 1\nmod = \"example:a\"\nprovides = [\"example:b\"]\nreplaces = [\"example:b\"]"
            ),
            OverlayError::ProvidesAndReplaces { .. }
        ));
        let entry = "schema = 1\nmod = \"example:a\"";
        assert!(matches!(
            Overlay::from_toml(&[entry, entry]),
            Err(OverlayError::DuplicateEntry(_))
        ));
    }
}
