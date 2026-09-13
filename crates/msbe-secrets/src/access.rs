//! What the provider policy gate may know about a user.

use std::collections::BTreeSet;

use msbe_core::config::Home;

use crate::{Acknowledgement, Acknowledgements, EnvironmentStore, StoreError, credentials, files};

/// Whether each provider has a credential, and which terms were acknowledged, without any secret.
///
/// This is all a policy gate needs. Loading it reads two small files and the environment; it never
/// contacts a keyring or unlocks the encrypted file.
#[derive(Debug, Default)]
pub struct Access {
    credentials: BTreeSet<String>,
    environment: Option<EnvironmentStore>,
    acknowledgements: Vec<Acknowledgement>,
}

impl Access {
    /// Nothing authenticated and nothing acknowledged.
    pub fn none() -> Self {
        Self::default()
    }

    /// What `home` records, with tokens from this process's environment.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when `credentials.toml` or `acknowledgements.toml` cannot be read.
    pub fn load(home: &Home) -> Result<Self, StoreError> {
        Self::load_with(home, EnvironmentStore::process())
    }

    /// What `home` records, with tokens from `environment`.
    ///
    /// # Errors
    ///
    /// As for [`Access::load`].
    pub fn load_with(home: &Home, environment: EnvironmentStore) -> Result<Self, StoreError> {
        Ok(Self {
            credentials: credentials::read_records(&files::auth_directory(home))?
                .into_iter()
                .map(|record| record.provider)
                .collect(),
            environment: Some(environment),
            acknowledgements: Acknowledgements::load(home)?.into_entries(),
        })
    }

    /// This access, with a credential for `provider` as well.
    #[must_use]
    pub fn with_credential(mut self, provider: impl Into<String>) -> Self {
        self.credentials.insert(provider.into());
        self
    }

    /// This access, with `acknowledgement` as well.
    #[must_use]
    pub fn with_acknowledgement(mut self, acknowledgement: Acknowledgement) -> Self {
        self.acknowledgements.push(acknowledgement);
        self
    }

    /// Whether `provider` has a stored credential or a token in the environment.
    pub fn is_authenticated(&self, provider: &str) -> bool {
        self.credentials.contains(provider)
            || self
                .environment
                .as_ref()
                .is_some_and(|environment| environment.contains(provider))
    }

    /// Whether `provider`'s current `terms`, under its current `program`, were acknowledged.
    pub fn has_acknowledged(&self, provider: &str, terms: &str, program: Option<&str>) -> bool {
        self.acknowledgements
            .iter()
            .any(|acknowledgement| acknowledgement.covers(provider, terms, program))
    }
}

#[cfg(test)]
mod tests {
    use msbe_core::config::Home;

    use super::Access;
    use crate::{
        Acknowledgement, Acknowledgements, Clock, Credentials, EnvironmentStore, MemoryStore,
        Secret,
    };

    #[derive(Debug)]
    struct Fixed;

    impl Clock for Fixed {
        fn now(&self) -> u64 {
            42
        }
    }

    #[test]
    fn access_reflects_recorded_credentials_tokens_and_acknowledgements() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        let mut credentials = Credentials::with_stores(
            &crate::auth_directory(&home),
            EnvironmentStore::with_lookup(|_| None),
            Some(Box::new(MemoryStore::default())),
            Box::new(Fixed),
        )
        .unwrap();
        credentials
            .store(
                "stored",
                &Secret::new("access-token-1".to_owned()).unwrap(),
                None,
            )
            .unwrap();
        Acknowledgements::load(&home)
            .unwrap()
            .acknowledge("stored", "https://example.test/terms", None, &Fixed)
            .unwrap();

        let access = Access::load_with(
            &home,
            EnvironmentStore::with_lookup(|provider| {
                (provider == "from-env").then(|| "environment-token-2".to_owned())
            }),
        )
        .unwrap();
        assert!(access.is_authenticated("stored"));
        assert!(access.is_authenticated("from-env"));
        assert!(!access.is_authenticated("other"));
        assert!(access.has_acknowledged("stored", "https://example.test/terms", None));
        assert!(!access.has_acknowledged("stored", "https://example.test/other", None));

        let none = Access::none();
        assert!(!none.is_authenticated("stored"));
        let granted = Access::none()
            .with_credential("example")
            .with_acknowledgement(Acknowledgement {
                provider: "example".to_owned(),
                terms: String::new(),
                program: Some("abc".to_owned()),
                acknowledged: 0,
            });
        assert!(granted.is_authenticated("example"));
        assert!(granted.has_acknowledged("example", "", Some("abc")));
        assert!(!granted.has_acknowledged("example", "", None));
    }
}
