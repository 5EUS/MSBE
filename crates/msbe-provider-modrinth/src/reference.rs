//! `project[@version]` references, and the characters that are safe in a request path.

use crate::ModrinthError;

/// A request for a project, optionally pinned to a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spec {
    /// The project's slug or id.
    pub(crate) project: String,
    /// A version number or version id to pin, if any.
    pub(crate) version: Option<String>,
}

impl Spec {
    /// Parses `project` or `project@version`.
    pub(crate) fn parse(raw: &str) -> Result<Self, ModrinthError> {
        let (project, version) = match raw.split_once('@') {
            Some((project, version)) => (project, Some(version)),
            None => (raw, None),
        };
        if !is_reference(project) || version.is_some_and(|version| !is_reference(version)) {
            return Err(ModrinthError::InvalidReference(raw.to_owned()));
        }
        Ok(Self {
            project: project.to_owned(),
            version: version.map(str::to_owned),
        })
    }
}

/// `raw`, when it is safe as one URL path segment.
pub(crate) fn segment(raw: &str) -> Result<&str, ModrinthError> {
    if is_reference(raw) {
        Ok(raw)
    } else {
        Err(ModrinthError::InvalidReference(raw.to_owned()))
    }
}

/// Whether `raw` can be a slug, id or version number, and is safe as one URL path segment.
fn is_reference(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 128
        && raw.chars().any(|c| c.is_ascii_alphanumeric())
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+'))
}

#[cfg(test)]
mod tests {
    use super::Spec;

    #[test]
    fn specs_accept_slugs_ids_and_pins_and_reject_path_characters() {
        assert_eq!(
            Spec::parse("sodium").unwrap(),
            Spec {
                project: "sodium".to_owned(),
                version: None
            }
        );
        assert_eq!(
            Spec::parse("sodium@mc1.21.1-0.8.13+fabric")
                .unwrap()
                .version
                .as_deref(),
            Some("mc1.21.1-0.8.13+fabric")
        );
        for bad in ["", "@1.0", "sodium@", "../etc", "a/b", "..", "sodium@1 0"] {
            assert!(Spec::parse(bad).is_err(), "{bad:?}");
        }
    }
}
