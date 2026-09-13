//! The HTTP surface providers depend on.

use std::{collections::BTreeMap, fmt, io::Write};

use thiserror::Error;

/// The HTTP operations providers need.
///
/// `msbe-http` implements this over the network. Tests implement it in memory, so provider
/// logic, version selection and download verification are all tested without a network.
pub trait HttpClient {
    /// Sends `request` and returns at most `request.limit` bytes of response body, with the quota
    /// the response reports in the headers `request.quota_headers` names.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError`] for a transport failure, a non-success status, rate limiting, a
    /// body larger than the limit, or a redirect that would carry a credential to another origin.
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError>;

    /// Streams the response body of `request` into `sink` and returns the number of bytes
    /// written.
    ///
    /// # Errors
    ///
    /// As for [`HttpClient::send`], plus a failure writing to `sink`.
    fn download(&self, request: &HttpRequest<'_>, sink: &mut dyn Write) -> Result<u64, HttpError>;
}

/// One request: where it goes, what it carries, and how much of the answer to accept.
#[derive(Debug, Clone)]
pub struct HttpRequest<'a> {
    /// The method, with the body a `POST` sends.
    pub method: Method<'a>,
    /// The URL, before `query` is appended.
    pub url: &'a str,
    /// Query parameters, percent-encoded and appended to `url`.
    pub query: &'a [(&'a str, &'a str)],
    /// Headers beyond the ones every request carries, such as the `User-Agent`.
    pub headers: Vec<Header<'a>>,
    /// Response headers that report remaining quota, lowercase.
    pub quota_headers: &'a [String],
    /// The most response body bytes to accept.
    pub limit: u64,
}

impl<'a> HttpRequest<'a> {
    /// A `GET` of `url`, accepting at most `limit` bytes of body.
    pub const fn get(url: &'a str, limit: u64) -> Self {
        Self::new(Method::Get, url, limit)
    }

    /// A `POST` of `json` to `url` as `application/json`, accepting at most `limit` bytes of
    /// response body. Used for queries too large for a URL, such as looking up many hashes at once.
    pub const fn post_json(url: &'a str, json: &'a [u8], limit: u64) -> Self {
        Self::new(Method::Post { json }, url, limit)
    }

    const fn new(method: Method<'a>, url: &'a str, limit: u64) -> Self {
        Self {
            method,
            url,
            query: &[],
            headers: Vec::new(),
            quota_headers: &[],
            limit,
        }
    }

    /// This request, with `query` appended to its URL.
    #[must_use]
    pub const fn with_query(mut self, query: &'a [(&'a str, &'a str)]) -> Self {
        self.query = query;
        self
    }

    /// This request, carrying `header` as well.
    #[must_use]
    pub fn with_header(mut self, header: Header<'a>) -> Self {
        self.headers.push(header);
        self
    }
}

/// An HTTP method, with the body it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method<'a> {
    /// `GET`, without a body.
    Get,
    /// `POST`, with a JSON body.
    Post {
        /// The body, sent as `application/json`.
        json: &'a [u8],
    },
}

/// A request header.
///
/// A credential's value never appears in `Debug` output, and a transport never sends it to
/// another origin, redirects included.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Header<'a> {
    /// The header name.
    pub name: &'a str,
    /// The header value.
    pub value: &'a str,
    /// Whether the value is a credential.
    pub credential: bool,
}

impl<'a> Header<'a> {
    /// A header whose value may be shown.
    pub const fn new(name: &'a str, value: &'a str) -> Self {
        Self {
            name,
            value,
            credential: false,
        }
    }

    /// A header carrying a credential.
    pub const fn credential(name: &'a str, value: &'a str) -> Self {
        Self {
            name,
            value,
            credential: true,
        }
    }
}

impl fmt::Debug for Header<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Header")
            .field("name", &self.name)
            .field(
                "value",
                if self.credential {
                    &"<redacted>"
                } else {
                    &self.value
                },
            )
            .finish()
    }
}

/// A response body and the quota the response reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpResponse {
    /// The body.
    pub body: Vec<u8>,
    /// The remaining quota, from the headers the request named.
    pub rate: Rate,
}

impl From<Vec<u8>> for HttpResponse {
    fn from(body: Vec<u8>) -> Self {
        Self {
            body,
            rate: Rate::default(),
        }
    }
}

/// The remaining quota a response reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rate {
    /// The remaining count, by the lowercase name of the header that reported it. A header that
    /// was absent or not a whole number is left out.
    pub remaining: BTreeMap<String, u64>,
}

/// The headers a provider's metadata API uses.
///
/// This names headers only. The provider registry holds the credential and attaches it, and only
/// to requests for the origin of the provider's `api_base`; never to an artifact download.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiHeaders {
    /// The header a credential is sent in, when the provider accepts one.
    pub credential: Option<String>,
    /// Response headers that report remaining quota.
    pub quota: Vec<String>,
}

impl ApiHeaders {
    /// Checks that every name is a lowercase header name, and that the credential header is not
    /// one the transport, a cookie jar or a proxy already gives a meaning to.
    ///
    /// # Errors
    ///
    /// Returns [`HeaderError`] naming the first unusable header.
    pub fn validate(&self) -> Result<(), HeaderError> {
        for name in self.credential.iter().chain(&self.quota) {
            if !is_header_name(name) {
                return Err(HeaderError::InvalidName(name.clone()));
            }
        }
        if let Some(name) = &self.credential
            && is_reserved(name)
        {
            return Err(HeaderError::Reserved(name.clone()));
        }
        Ok(())
    }
}

/// The longest header name a provider may declare.
const HEADER_NAME_LIMIT: usize = 64;

fn is_header_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= HEADER_NAME_LIMIT
        && bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Headers that already mean something to HTTP itself, so a credential in one would be sent,
/// logged, forwarded or dropped by rules MSBE does not control.
fn is_reserved(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "connection"
            | "cookie"
            | "expect"
            | "host"
            | "keep-alive"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "user-agent"
    ) || name.starts_with("content-")
        || name.starts_with("proxy-")
}

/// Why a declared header cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum HeaderError {
    /// The name is not lowercase ASCII letters, digits and hyphens, starting with a letter.
    #[error("{0:?} is not a lowercase header name")]
    InvalidName(String),
    /// The name already has a meaning in HTTP, so it cannot carry a credential.
    #[error("{0:?} cannot carry a credential")]
    Reserved(String),
}

/// The scheme, host and port of a URL: what decides where a credential may be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    scheme: &'static str,
    host: String,
    port: u16,
}

impl Origin {
    /// The origin of an absolute `http` or `https` URL.
    ///
    /// Returns `None` for any other URL, including one with user info, a backslash or
    /// percent-encoding in its authority, or a host that is not plain ASCII, so a URL two parsers
    /// could read differently never matches.
    pub fn of(url: &str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        let (scheme, default_port) = if scheme.eq_ignore_ascii_case("https") {
            ("https", 443)
        } else if scheme.eq_ignore_ascii_case("http") {
            ("http", 80)
        } else {
            return None;
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            let (address, after) = bracketed.split_once(']')?;
            let valid = !address.is_empty()
                && address
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() || matches!(byte, b':' | b'.'));
            (valid.then(|| format!("[{address}]"))?, after)
        } else {
            let end = authority.find(':').unwrap_or(authority.len());
            let (host, after) = authority.split_at(end);
            let valid = !host.is_empty()
                && host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'));
            (valid.then(|| host.to_owned())?, after)
        };
        let port = match port {
            "" => default_port,
            explicit => explicit
                .strip_prefix(':')
                .filter(|digits| digits.bytes().all(|byte| byte.is_ascii_digit()))?
                .parse()
                .ok()?,
        };
        Some(Self {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
        })
    }

    /// Whether `url` has this origin.
    pub fn serves(&self, url: &str) -> bool {
        Self::of(url).is_some_and(|origin| origin == *self)
    }
}

/// `url` without its query or fragment, for an error. Signed download links carry their
/// credentials in the query, and an error message is somewhere they must not go
/// (`docs/07-browser-and-secrets.md` §7.5).
pub fn without_query(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
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

    /// A request carrying a credential was redirected to another origin, and was not followed.
    #[error(
        "{url} redirected to {location}, another origin, where a request carrying a credential is not followed"
    )]
    CrossOriginRedirect {
        /// The redirected request's URL.
        url: String,
        /// Where it was redirected to.
        location: String,
    },

    /// The credential a request needs could not be read.
    #[error("cannot read the credential for {url}: {reason}")]
    Credential {
        /// The request URL.
        url: String,
        /// Why the credential could not be read.
        reason: String,
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

#[cfg(test)]
mod tests {
    use super::{ApiHeaders, Header, HeaderError, Origin, without_query};

    #[test]
    fn origins_compare_scheme_host_and_port_after_defaults_and_case() {
        let api = Origin::of("https://api.example.test/v1").unwrap();
        for same in [
            "https://api.example.test",
            "HTTPS://API.Example.TEST:443/v1/files?key=1",
            "https://api.example.test#fragment",
            "https://api.example.test?query",
        ] {
            assert!(api.serves(same), "{same}");
        }
        for other in [
            "http://api.example.test/v1",
            "https://api.example.test:8443/v1",
            "https://cdn.example.test/v1",
            "https://api.example.test.other.test/v1",
            "https://api.example.test@other.test/v1",
            "https://other.test\\@api.example.test/v1",
            "https://api%2eexample.test/v1",
            "https://api.example.test:/v1",
            "https://api.example.test:+443/v1",
            "https://api.example.test:99999/v1",
            "ftp://api.example.test/v1",
            "//api.example.test/v1",
            "/v1/files",
            "",
        ] {
            assert!(!api.serves(other), "{other}");
        }

        let loopback = Origin::of("http://[::1]:8080/").unwrap();
        assert!(loopback.serves("http://[::1]:8080/other"));
        assert!(!loopback.serves("http://[::1]/"));
        assert_eq!(Origin::of("http://[::1/"), None);
        assert_eq!(Origin::of("http://[]/"), None);
    }

    #[test]
    fn credential_headers_are_lowercase_names_http_gives_no_other_meaning() {
        let headers = |credential: &str| ApiHeaders {
            credential: Some(credential.to_owned()),
            quota: vec!["x-rate-remaining".to_owned()],
        };
        for accepted in ["apikey", "x-api-key", "private-token"] {
            assert_eq!(headers(accepted).validate(), Ok(()), "{accepted}");
        }
        for reserved in [
            "authorization",
            "cookie",
            "host",
            "user-agent",
            "content-type",
            "content-length",
            "proxy-authorization",
            "transfer-encoding",
        ] {
            assert_eq!(
                headers(reserved).validate(),
                Err(HeaderError::Reserved(reserved.to_owned()))
            );
        }
        for invalid in ["", "X-Api-Key", "x api key", "x_api_key", "1key", "x:key"] {
            assert_eq!(
                headers(invalid).validate(),
                Err(HeaderError::InvalidName(invalid.to_owned()))
            );
        }
        let quota = ApiHeaders {
            credential: None,
            quota: vec!["X-Remaining".to_owned()],
        };
        assert!(matches!(quota.validate(), Err(HeaderError::InvalidName(_))));
        assert_eq!(ApiHeaders::default().validate(), Ok(()));
    }

    #[test]
    fn a_credential_header_never_shows_its_value() {
        let shown = format!("{:?}", Header::new("accept", "application/json"));
        assert!(shown.contains("application/json"), "{shown}");
        let hidden = format!("{:?}", Header::credential("x-api-key", "credential-value"));
        assert!(hidden.contains("x-api-key"), "{hidden}");
        assert!(!hidden.contains("credential-value"), "{hidden}");
    }

    #[test]
    fn urls_in_errors_lose_their_query_and_fragment() {
        assert_eq!(
            without_query("https://example.test/a?key=secret#part"),
            "https://example.test/a"
        );
        assert_eq!(
            without_query("https://example.test/a#key=secret"),
            "https://example.test/a"
        );
    }
}
