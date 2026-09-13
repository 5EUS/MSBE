//! The secret value type.

use std::fmt;

use thiserror::Error;
use zeroize::Zeroizing;

use crate::redact;

/// The fewest characters a secret may have. A shorter value could not be redacted without also
/// hiding ordinary words.
pub const MIN_SECRET_LEN: usize = 8;

/// The most bytes a secret may have.
pub const MAX_SECRET_LEN: usize = 4096;

/// A credential, such as an API key or a passphrase.
///
/// The value is zeroized when dropped, `Debug` never shows it, and there is no `Display`,
/// `Serialize` or `Clone`. Creating one registers the value with [`redact`], so it is removed from
/// everything the daemon writes for the rest of the process.
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Takes `value` as a secret.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError`] for a value shorter than [`MIN_SECRET_LEN`] characters or longer
    /// than [`MAX_SECRET_LEN`] bytes, or one with surrounding whitespace or control characters. A
    /// refused value is zeroized too.
    pub fn new(value: String) -> Result<Self, SecretError> {
        let value = Zeroizing::new(value);
        if value.len() > MAX_SECRET_LEN {
            return Err(SecretError::TooLong);
        }
        if value.chars().count() < MIN_SECRET_LEN {
            return Err(SecretError::TooShort);
        }
        if value.trim() != value.as_str() {
            return Err(SecretError::Whitespace);
        }
        if value.chars().any(char::is_control) {
            return Err(SecretError::Control);
        }
        redact::register(&value);
        Ok(Self(value))
    }

    /// The value, for the one call that sends or stores it. Never format it into a message.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

/// Why a value cannot be a secret. The value itself is never part of the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SecretError {
    /// The value is too short to redact safely.
    #[error("a secret must be at least {} characters", MIN_SECRET_LEN)]
    TooShort,
    /// The value is longer than any credential MSBE keeps.
    #[error("a secret must be at most {} bytes", MAX_SECRET_LEN)]
    TooLong,
    /// The value begins or ends with whitespace, which is usually a copy-and-paste accident.
    #[error("a secret must not begin or end with whitespace")]
    Whitespace,
    /// The value contains a control character, which no request header may carry.
    #[error("a secret must not contain control characters")]
    Control,
}

#[cfg(test)]
mod tests {
    use super::{MAX_SECRET_LEN, Secret, SecretError};

    #[test]
    fn debug_output_never_shows_the_value() {
        let secret = Secret::new("hunter2-but-longer".to_owned()).unwrap();
        assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
        assert_eq!(secret.expose(), "hunter2-but-longer");
    }

    #[test]
    fn values_that_cannot_be_redacted_or_sent_are_refused() {
        for (value, error) in [
            ("short", SecretError::TooShort),
            (" padded-value", SecretError::Whitespace),
            ("trailing-newline\n", SecretError::Whitespace),
            ("tab\tinside-it", SecretError::Control),
        ] {
            assert_eq!(Secret::new(value.to_owned()).unwrap_err(), error);
        }
        assert_eq!(
            Secret::new("x".repeat(MAX_SECRET_LEN + 1)).unwrap_err(),
            SecretError::TooLong
        );
    }
}
