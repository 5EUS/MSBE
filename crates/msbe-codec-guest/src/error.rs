//! Codec failures, in the shape the host maps to its typed codec errors.

use std::fmt;

use serde::Serialize;

/// Why a codec operation failed. The host maps each kind to its own typed error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Error {
    /// Supplied options are invalid.
    InvalidOptions {
        /// What is wrong.
        message: String,
    },
    /// The codec does not run in this direction: `import` or `export`.
    UnsupportedDirection {
        /// The refused direction.
        direction: String,
    },
    /// The codec cannot represent this plan or loader.
    UnsupportedTarget {
        /// The plan ID.
        game: String,
        /// The loader ID.
        loader: String,
    },
    /// The pack is not in this codec's format.
    FormatMismatch,
    /// An entry exceeds a read limit.
    Limit {
        /// What exceeded which limit.
        message: String,
    },
    /// An entry path is unsafe.
    UnsafePath {
        /// The refused path.
        message: String,
    },
    /// A required blob is unavailable.
    MissingBlob {
        /// Its digest, as `sha256:<hex>`.
        digest: String,
    },
    /// Content cannot be represented reproducibly.
    Unreproducible {
        /// What cannot be reproduced.
        message: String,
    },
    /// Distribution policy prohibits embedding content.
    DistributionForbidden {
        /// What may not be embedded.
        message: String,
    },
    /// Any other codec failure.
    Codec {
        /// What went wrong.
        message: String,
    },
}

impl Error {
    /// A general codec failure described by `message`.
    pub fn codec(message: impl fmt::Display) -> Self {
        Self::Codec {
            message: message.to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOptions { message } => write!(formatter, "invalid options: {message}"),
            Self::UnsupportedDirection { direction } => {
                write!(formatter, "the codec does not support {direction}")
            }
            Self::UnsupportedTarget { game, loader } => {
                write!(formatter, "the codec does not support {game} with {loader}")
            }
            Self::FormatMismatch => formatter.write_str("the pack is not in this format"),
            Self::Limit { message }
            | Self::UnsafePath { message }
            | Self::Unreproducible { message }
            | Self::DistributionForbidden { message }
            | Self::Codec { message } => formatter.write_str(message),
            Self::MissingBlob { digest } => write!(formatter, "blob {digest} is unavailable"),
        }
    }
}

impl std::error::Error for Error {}
