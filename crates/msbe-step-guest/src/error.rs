//! Step failures, in the shape the host reports.

use std::fmt;

use serde::Serialize;

/// Why a step failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Error {
    /// The mod is malformed for this step.
    InvalidArchive {
        /// What is wrong.
        message: String,
    },
    /// A question has no recorded answer and no default. The host collects it for the user.
    QuestionPending {
        /// The question's identifier.
        question: String,
    },
    /// The host refused a read the step's declaration does not grant.
    Denied {
        /// What was refused.
        message: String,
    },
    /// A read exceeded its limit or the host's read budget.
    Limit {
        /// What exceeded which limit.
        message: String,
    },
    /// A path was not a safe relative path.
    UnsafePath {
        /// The refused path.
        message: String,
    },
    /// Any other failure.
    Extension {
        /// What went wrong.
        message: String,
    },
}

impl Error {
    /// A general failure described by `message`.
    pub fn extension(message: impl fmt::Display) -> Self {
        Self::Extension {
            message: message.to_string(),
        }
    }

    /// A malformed mod, described by `message`.
    pub fn invalid_archive(message: impl fmt::Display) -> Self {
        Self::InvalidArchive {
            message: message.to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QuestionPending { question } => {
                write!(formatter, "question {question} has no answer")
            }
            Self::InvalidArchive { message }
            | Self::Denied { message }
            | Self::Limit { message }
            | Self::UnsafePath { message }
            | Self::Extension { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for Error {}
