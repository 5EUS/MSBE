//! Canonical, signed extension envelopes shared by reviewed extension types.

use std::collections::BTreeMap;

use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::hex;

/// A signed extension with bounded compatibility claims and a typed payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionEnvelope<T> {
    /// Envelope schema version.
    pub schema: u32,
    /// Lowercase SHA-256 of deterministic payload bytes.
    pub package_digest: String,
    /// Stable extension identifier.
    pub id: String,
    /// Stable extension version.
    pub version: String,
    /// Reviewed extension interfaces the package provides.
    pub provides: Vec<ExtensionProvide>,
    /// Host API versions supported by this extension.
    pub host_api: HostApiRange,
    /// Capabilities granted to the reviewed runtime.
    pub capabilities: Vec<ExtensionCapability>,
    /// Stable signer identity selected from the registry trust root.
    pub signer: String,
    /// Lowercase hexadecimal Ed25519 signature over the canonical envelope.
    pub signature: String,
    /// The typed payload a reviewed runtime interprets.
    pub payload: T,
}

impl<T: Serialize> ExtensionEnvelope<T> {
    /// Computes the normalized SHA-256 package digest for `payload`.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError::Canonical`] when the payload cannot be serialized.
    pub fn package_digest_for(payload: &T) -> Result<String, EnvelopeError> {
        let canonical = serde_json::to_vec(payload)
            .map_err(|error| EnvelopeError::Canonical(error.to_string()))?;
        Ok(hex(&Sha256::digest(canonical)))
    }

    /// Signs this envelope with `key` after checking its declared package digest.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError`] when the envelope fails [`Self::validate`] or cannot be
    /// serialized.
    pub fn sign(&mut self, key: &SigningKey) -> Result<(), EnvelopeError> {
        self.validate()?;
        self.signature = hex(&key.sign(&self.canonical_signed_payload()?).to_bytes());
        Ok(())
    }

    /// Verifies this envelope against a signer-to-verifying-key trust store.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError`] when the envelope is invalid, its signer is untrusted, or its
    /// signature does not verify.
    pub fn verify(
        &self,
        trusted_keys: &BTreeMap<String, VerifyingKey>,
    ) -> Result<(), EnvelopeError> {
        self.validate()?;
        let key = trusted_keys
            .get(&self.signer)
            .ok_or_else(|| EnvelopeError::UnknownSigner(self.signer.clone()))?;
        let signature = Signature::from_slice(&decode_hex(&self.signature, 64)?)
            .map_err(|_| EnvelopeError::InvalidSignature)?;
        key.verify(&self.canonical_signed_payload()?, &signature)
            .map_err(|_| EnvelopeError::SignatureMismatch)
    }

    /// Checks structural safety and the declared normalized payload digest.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError`] naming the first structural, digest, or signature-shape problem.
    pub fn validate(&self) -> Result<(), EnvelopeError> {
        if self.schema != 1 {
            return Err(EnvelopeError::UnsupportedSchema(self.schema));
        }
        validate_text("extension id", &self.id)?;
        validate_text("extension version", &self.version)?;
        if self.provides.is_empty() || has_adjacent_duplicate(&self.provides) {
            return Err(EnvelopeError::InvalidProvides);
        }
        if self.host_api.minimum > self.host_api.maximum {
            return Err(EnvelopeError::InvalidHostApiRange);
        }
        if has_adjacent_duplicate(&self.capabilities) {
            return Err(EnvelopeError::DuplicateCapability);
        }
        validate_text("signer", &self.signer)?;
        let actual = Self::package_digest_for(&self.payload)?;
        if self.package_digest != actual {
            return Err(EnvelopeError::DigestMismatch {
                declared: self.package_digest.clone(),
                actual,
            });
        }
        drop(decode_hex(&self.signature, 64)?);
        Ok(())
    }

    fn canonical_signed_payload(&self) -> Result<Vec<u8>, EnvelopeError> {
        serde_json::to_vec(&SignedExtension {
            schema: self.schema,
            package_digest: &self.package_digest,
            id: &self.id,
            version: &self.version,
            provides: &self.provides,
            host_api: &self.host_api,
            capabilities: &self.capabilities,
            signer: &self.signer,
            payload: &self.payload,
        })
        .map_err(|error| EnvelopeError::Canonical(error.to_string()))
    }
}

#[derive(Serialize)]
struct SignedExtension<'a, T> {
    schema: u32,
    package_digest: &'a str,
    id: &'a str,
    version: &'a str,
    provides: &'a [ExtensionProvide],
    host_api: &'a HostApiRange,
    capabilities: &'a [ExtensionCapability],
    signer: &'a str,
    payload: &'a T,
}

/// A closed vocabulary of extension interfaces implemented by a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionProvide {
    /// A declarative provider program interpreted by a reviewed host runtime.
    ProviderProgramV1,
    /// A sandboxed pack codec served by the reviewed WebAssembly host.
    PackCodecV1,
}

/// A closed vocabulary of runtime powers an extension may request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExtensionCapability {
    /// Make HTTPS requests through the host's reviewed transport.
    Network,
}

/// Inclusive supported host API range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostApiRange {
    /// Lowest supported host API version.
    pub minimum: u32,
    /// Highest supported host API version.
    pub maximum: u32,
}

fn validate_text(kind: &'static str, value: &str) -> Result<(), EnvelopeError> {
    if value.trim().is_empty() || !value.is_ascii() {
        return Err(EnvelopeError::InvalidText {
            kind,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn has_adjacent_duplicate<T: PartialEq>(values: &[T]) -> bool {
    values
        .windows(2)
        .any(|pair| matches!(pair, [left, right] if left == right))
}

fn decode_hex(value: &str, expected_bytes: usize) -> Result<Vec<u8>, EnvelopeError> {
    if value.len() != expected_bytes * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(EnvelopeError::InvalidSignature);
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|digits| u8::from_str_radix(digits, 16).ok())
                .ok_or(EnvelopeError::InvalidSignature)
        })
        .collect()
}

/// Why an extension envelope was refused before its payload could run.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EnvelopeError {
    /// The envelope schema is unsupported.
    #[error("unsupported extension envelope schema {0}")]
    UnsupportedSchema(u32),
    /// An identity field is empty or not ASCII.
    #[error("invalid {kind} {value:?}")]
    InvalidText {
        /// Identity field that failed validation.
        kind: &'static str,
        /// Rejected field value.
        value: String,
    },
    /// No reviewed runtime supports the declared interface set.
    #[error("extension must declare a non-duplicated provide")]
    InvalidProvides,
    /// The host API range is inverted.
    #[error("extension host API range is inverted")]
    InvalidHostApiRange,
    /// A capability appears more than once.
    #[error("extension capability appears more than once")]
    DuplicateCapability,
    /// The normalized payload digest did not match.
    #[error("extension digest mismatch: declared {declared}, computed {actual}")]
    DigestMismatch {
        /// Digest declared in the envelope.
        declared: String,
        /// Digest computed from the normalized payload.
        actual: String,
    },
    /// The signature is malformed.
    #[error("invalid extension Ed25519 signature")]
    InvalidSignature,
    /// No verifying key is trusted for the signer.
    #[error("extension signer {0:?} is not trusted")]
    UnknownSigner(String),
    /// The signature did not verify.
    #[error("extension Ed25519 signature did not verify")]
    SignatureMismatch,
    /// Canonical serialization failed.
    #[error("cannot canonicalize extension: {0}")]
    Canonical(String),
}
