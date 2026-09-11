//! The HTTP surface providers depend on.

use std::io::Write;

use thiserror::Error;

/// The HTTP operations providers need.
///
/// `msbe-http` implements this over the network. Tests implement it in memory, so provider
/// logic, version selection and download verification are all tested without a network.
pub trait HttpClient {
    /// Fetches `url` with `query` appended and returns at most `limit` bytes of body.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError`] for a transport failure, a non-success status, rate limiting, or
    /// a body larger than `limit`.
    fn get(&self, url: &str, query: &[(&str, &str)], limit: u64) -> Result<Vec<u8>, HttpError>;

    /// Streams the body of `url` into `sink` and returns the number of bytes written.
    ///
    /// # Errors
    ///
    /// As for [`HttpClient::get`], plus a failure writing to `sink`.
    fn download(&self, url: &str, sink: &mut dyn Write, limit: u64) -> Result<u64, HttpError>;
}

/// Why an HTTP request failed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HttpError {
    /// The server answered with a status other than success.
    #[error("{url} answered HTTP {status}")]
    Status {
        /// The request URL.
        url: String,
        /// The HTTP status code.
        status: u16,
    },

    /// The server is rate limiting this client.
    #[error("{url} is rate limiting requests; try again shortly")]
    RateLimited {
        /// The request URL.
        url: String,
        /// Seconds until the limit resets, when the server said.
        retry_after: Option<u64>,
    },

    /// The response was larger than allowed.
    #[error("{url} sent more than {limit} bytes")]
    TooLarge {
        /// The request URL.
        url: String,
        /// The limit in bytes.
        limit: u64,
    },

    /// The request could not be completed.
    #[error("{url}: {message}")]
    Transport {
        /// The request URL.
        url: String,
        /// What went wrong.
        message: String,
    },
}
