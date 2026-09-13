//! Tokens from the environment: the CI and container path.

use std::fmt;

use zeroize::Zeroizing;

use crate::{Backend, Secret, SecretStore, StoreError, store::validate_provider};

type Lookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Reads a provider's token from `MSBE_<PROVIDER>_TOKEN`, the provider id uppercased with each `-`
/// written as `_`. It can neither keep nor forget a token.
pub struct EnvironmentStore {
    lookup: Lookup,
}

impl EnvironmentStore {
    /// Reads this process's environment, through `msbe_core::config`.
    pub fn process() -> Self {
        Self::with_lookup(msbe_core::config::provider_token)
    }

    /// Reads tokens with `lookup`, which is given a provider id.
    pub fn with_lookup(lookup: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Self {
        Self {
            lookup: Box::new(lookup),
        }
    }

    /// Whether a token is set for `provider`. The token is not kept.
    pub fn contains(&self, provider: &str) -> bool {
        if validate_provider(provider).is_err() {
            return false;
        }
        let token = (self.lookup)(provider).map(Zeroizing::new);
        token.is_some()
    }
}

impl fmt::Debug for EnvironmentStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EnvironmentStore")
            .finish_non_exhaustive()
    }
}

impl SecretStore for EnvironmentStore {
    fn backend(&self) -> Backend {
        Backend::Environment
    }

    fn get(&self, provider: &str) -> Result<Option<Secret>, StoreError> {
        validate_provider(provider)?;
        (self.lookup)(provider)
            .map(|token| {
                Secret::new(token).map_err(|error| StoreError::InvalidEnvironment {
                    variable: msbe_core::config::provider_token_variable(provider),
                    error,
                })
            })
            .transpose()
    }

    fn set(&mut self, _: &str, _: &Secret) -> Result<(), StoreError> {
        Err(StoreError::ReadOnly(Backend::Environment))
    }

    fn delete(&mut self, _: &str) -> Result<bool, StoreError> {
        Err(StoreError::ReadOnly(Backend::Environment))
    }
}

#[cfg(test)]
mod tests {
    use super::EnvironmentStore;
    use crate::{Secret, SecretError, SecretStore, StoreError};

    #[test]
    fn tokens_are_read_by_provider_and_never_written() {
        let mut store = EnvironmentStore::with_lookup(|provider| match provider {
            "example-tool" => Some("environment-token-1".to_owned()),
            "short" => Some("abc".to_owned()),
            _ => None,
        });
        assert!(store.contains("example-tool"));
        assert!(!store.contains("other"));
        assert_eq!(
            store.get("example-tool").unwrap().unwrap().expose(),
            "environment-token-1"
        );
        assert!(store.get("other").unwrap().is_none());
        assert!(matches!(
            store.get("short"),
            Err(StoreError::InvalidEnvironment { variable, error: SecretError::TooShort })
                if variable == "MSBE_SHORT_TOKEN"
        ));
        let secret = Secret::new("cannot-be-kept".to_owned()).unwrap();
        assert!(matches!(
            store.set("example-tool", &secret),
            Err(StoreError::ReadOnly(_))
        ));
        assert!(matches!(
            store.delete("example-tool"),
            Err(StoreError::ReadOnly(_))
        ));
    }

    #[test]
    fn variable_names_uppercase_the_provider_id() {
        assert_eq!(
            msbe_core::config::provider_token_variable("example-tool"),
            "MSBE_EXAMPLE_TOOL_TOKEN"
        );
    }
}
