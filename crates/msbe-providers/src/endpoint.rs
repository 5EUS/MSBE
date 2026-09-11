//! Bounded HTTPS JSON endpoints for reviewed provider adapters.

use std::fmt;

use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::HttpClient;

/// A provider metadata endpoint with one trusted HTTPS base URL and a response size limit.
pub struct JsonEndpoint<'a> {
    http: &'a dyn HttpClient,
    base: String,
    limit: u64,
}

impl fmt::Debug for JsonEndpoint<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonEndpoint")
            .field("base", &self.base)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl<'a> JsonEndpoint<'a> {
    /// Creates an endpoint over `base`.
    ///
    /// Provider manifests validate the base URL before an adapter constructs this helper.
    pub(crate) fn new(http: &'a dyn HttpClient, base: impl Into<String>, limit: u64) -> Self {
        Self {
            http,
            base: base.into(),
            limit,
        }
    }

    /// Fetches and decodes JSON from an endpoint-relative path.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError`] for an unsafe path, HTTP failure, or invalid JSON response.
    pub fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, EndpointError> {
        let url = self.url(path)?;
        let body = self.http.get(&url, query, self.limit)?;
        decode(url, &body)
    }

    /// Sends a JSON request and decodes the JSON response from an endpoint-relative path.
    ///
    /// # Errors
    ///
    /// Returns [`EndpointError`] for an unsafe path, serialization or HTTP failure, or invalid
    /// JSON response.
    pub fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        request: &impl Serialize,
    ) -> Result<T, EndpointError> {
        let url = self.url(path)?;
        let body = serde_json::to_vec(request).map_err(EndpointError::Encode)?;
        let response = self.http.post_json(&url, &body, self.limit)?;
        decode(url, &response)
    }

    fn url(&self, path: &str) -> Result<String, EndpointError> {
        if !path.starts_with('/')
            || path.contains("//")
            || path.split('/').any(|part| part == "." || part == "..")
        {
            return Err(EndpointError::InvalidPath(path.to_owned()));
        }
        Ok(format!("{}{path}", self.base.trim_end_matches('/')))
    }

    pub(crate) fn http(&self) -> &dyn HttpClient {
        self.http
    }
}

fn decode<T: DeserializeOwned>(url: String, body: &[u8]) -> Result<T, EndpointError> {
    serde_json::from_slice(body).map_err(|source| EndpointError::Decode {
        url,
        reason: source.to_string(),
    })
}

/// Why an endpoint request could not be completed.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EndpointError {
    /// The adapter attempted to escape its declared metadata endpoint.
    #[error("invalid endpoint-relative path {0:?}")]
    InvalidPath(String),
    /// JSON encoding failed.
    #[error("cannot encode endpoint request: {0}")]
    Encode(#[source] serde_json::Error),
    /// The HTTP request failed.
    #[error(transparent)]
    Http(#[from] crate::HttpError),
    /// The response body was not the expected JSON type.
    #[error("unexpected response from {url}: {reason}")]
    Decode {
        /// The requested URL.
        url: String,
        /// The decoder's message.
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;

    use super::{EndpointError, JsonEndpoint};
    use crate::{HttpClient, HttpError};

    struct Fake;

    impl HttpClient for Fake {
        fn get(&self, _: &str, _: &[(&str, &str)], _: u64) -> Result<Vec<u8>, HttpError> {
            Ok(br#"{"value":1}"#.to_vec())
        }

        fn post_json(&self, _: &str, _: &[u8], _: u64) -> Result<Vec<u8>, HttpError> {
            Ok(br#"{"value":1}"#.to_vec())
        }

        fn download(&self, _: &str, _: &mut dyn Write, _: u64) -> Result<u64, HttpError> {
            Err(HttpError::Status {
                url: "test".to_owned(),
                status: 404,
            })
        }
    }

    #[test]
    fn joins_only_safe_relative_paths() -> Result<(), EndpointError> {
        let endpoint = JsonEndpoint::new(&Fake, "https://api.example.test/v1/", 1024);
        let response: serde_json::Value = endpoint.get("/projects", &[])?;
        assert_eq!(response, json!({"value": 1}));
        assert!(matches!(
            endpoint.get::<serde_json::Value>("projects", &[]),
            Err(EndpointError::InvalidPath(_))
        ));
        assert!(matches!(
            endpoint.get::<serde_json::Value>("/../projects", &[]),
            Err(EndpointError::InvalidPath(_))
        ));
        Ok(())
    }
}
