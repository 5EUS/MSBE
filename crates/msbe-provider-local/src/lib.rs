//! Local content and the provider-neutral native `.msbepack` codec.
//!
//! Local files are ingested by the host from paths the user chose, so this extension has no
//! remote protocol: its adapter refuses provider references and exists so the native codec joins
//! MSBE through the same reviewed registration as every other format
//! (`docs/17-pack-formats-and-native-bundles.md` §17.2).

mod native;

use msbe_provider_api::{
    Adapter, AdapterError, PackCodec, PackCodecError, PackCodecRegistration, Registration,
    model::Request,
};
use thiserror::Error;

pub use native::{CODEC_ID, NativeCodec};

/// The provider id the local manifest declares.
pub const ID: &str = "local";

/// How local content and the native bundle codec join MSBE.
pub const REGISTRATION: Registration = Registration {
    id: ID,
    manifest: include_str!("../manifest.toml"),
    overlay: &[],
    build: |_| Ok(Box::new(Local)),
    pack_codecs: &[PackCodecRegistration {
        id: CODEC_ID,
        build: build_native_codec,
    }],
    exception_reason: "Local content is ingested by the host from user-selected paths and has no remote protocol; the native bundle codec is reviewed host-format code.",
};

#[expect(
    clippy::unnecessary_wraps,
    reason = "codec builders implement the fallible registration function pointer"
)]
fn build_native_codec() -> Result<Box<dyn PackCodec>, PackCodecError> {
    Ok(Box::new(NativeCodec::new()))
}

/// The local content adapter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Local;

impl Adapter for Local {
    fn id(&self) -> &str {
        ID
    }

    fn request(&self, reference: &str) -> Result<Request, AdapterError> {
        Err(AdapterError::specific(LocalError::HostIngested(
            reference.to_owned(),
        )))
    }
}

/// Why a local reference was refused.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LocalError {
    /// Local content is added from its path rather than acquired through a reference.
    #[error(
        "local content {0:?} is added from its path, not acquired through a provider reference"
    )]
    HostIngested(String),
}

#[cfg(test)]
mod tests {
    use msbe_provider_api::Adapter as _;

    use super::{Local, REGISTRATION};

    #[test]
    fn registration_exposes_only_the_native_codec() {
        assert_eq!(REGISTRATION.pack_codecs.len(), 1);
        assert_eq!(
            REGISTRATION.pack_codecs.first().map(|codec| codec.id),
            Some("msbe-native")
        );
        assert!(Local.request("/tmp/example.jar").is_err());
    }
}
