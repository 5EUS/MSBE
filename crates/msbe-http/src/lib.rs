//! The network transport for providers: HTTPS over rustls with the operating system's trust
//! store.
//!
//! This is the only crate that links TLS. Keeping it separate lets every other crate stay
//! plain Rust: providers are tested against in-memory [`HttpClient`] fakes, and the rest of the
//! workspace can be linted for Windows on machines without the MSVC toolchain `ring` needs.
//!
//! Root certificates come from the operating system rather than a bundled list. That respects
//! system and corporate certificate authorities, and keeps `webpki-roots` (licensed
//! CDLA-Permissive-2.0, which `deny.toml` does not allow) out of the dependency graph.

use std::{fmt, io, io::Write, sync::Arc, time::Duration};

use msbe_provider_api::{HttpClient, HttpError};
use ureq::{
    Agent, Body,
    http::{Response, StatusCode},
    tls::{Certificate, RootCerts, TlsConfig, TlsProvider},
};

/// The `User-Agent` every request carries. Catalogs require one that identifies the application,
/// not just the HTTP library.
pub const USER_AGENT: &str = concat!(
    "5EUS/MSBE/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/5EUS/MSBE)"
);

/// How long to wait for a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The longest one request, including its body, may take. Large mods on slow links need room.
const REQUEST_TIMEOUT: Duration = Duration::from_mins(15);

/// An HTTPS client for providers.
pub struct UreqClient {
    agent: Agent,
}

impl fmt::Debug for UreqClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UreqClient").finish_non_exhaustive()
    }
}

impl UreqClient {
    /// Builds a client that trusts the operating system's certificate authorities and speaks
    /// only `https`, redirects included, so a redirect cannot downgrade a download to `http`.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError::Transport`] if the trust store yields no usable certificates.
    pub fn connect() -> Result<Self, HttpError> {
        Self::build(true)
    }

    fn build(https_only: bool) -> Result<Self, HttpError> {
        let loaded = rustls_native_certs::load_native_certs();
        let roots: Vec<Certificate<'static>> = loaded
            .certs
            .iter()
            .map(|der| Certificate::from_der(der.as_ref()).to_owned())
            .collect();
        if roots.is_empty() {
            let detail = loaded
                .errors
                .first()
                .map_or_else(|| "none were found".to_owned(), ToString::to_string);
            return Err(HttpError::Transport {
                url: "the system trust store".to_owned(),
                message: format!("no usable root certificates: {detail}"),
            });
        }

        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .root_certs(RootCerts::Specific(Arc::new(roots)))
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        let config = Agent::config_builder()
            .user_agent(USER_AGENT)
            .tls_config(tls)
            // Statuses are inspected here so a 429's Retry-After survives.
            .http_status_as_error(false)
            .https_only(https_only)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build();
        Ok(Self {
            agent: Agent::new_with_config(config),
        })
    }
}

impl HttpClient for UreqClient {
    fn get(&self, url: &str, query: &[(&str, &str)], limit: u64) -> Result<Vec<u8>, HttpError> {
        let sent = self
            .agent
            .get(url)
            .query_pairs(query.iter().copied())
            .call();
        read_limited(url, checked(url, sent)?, limit)
    }

    fn post_json(&self, url: &str, body: &[u8], limit: u64) -> Result<Vec<u8>, HttpError> {
        let sent = self
            .agent
            .post(url)
            .content_type("application/json")
            .send(body);
        read_limited(url, checked(url, sent)?, limit)
    }

    fn download(&self, url: &str, sink: &mut dyn Write, limit: u64) -> Result<u64, HttpError> {
        let mut reader = checked(url, self.agent.get(url).call())?
            .into_body()
            .into_with_config()
            .limit(reader_limit(limit))
            .reader();
        let written = io::copy(&mut reader, sink)
            .map_err(|error| transport(url, &ureq::Error::from(error)))?;
        if written > limit {
            return Err(too_large(url, limit));
        }
        Ok(written)
    }
}

/// ureq's limit refuses a body whose length reaches the limit, so a response of exactly
/// `limit` bytes would fail. Give ureq one byte of headroom as a backstop against unbounded
/// bodies, and enforce "at most `limit` bytes" here, where the rule is ours.
const fn reader_limit(limit: u64) -> u64 {
    limit.saturating_add(1)
}

/// Maps a transport failure, rate limiting, or a non-success status to an [`HttpError`].
fn checked(
    url: &str,
    sent: Result<Response<Body>, ureq::Error>,
) -> Result<Response<Body>, HttpError> {
    let response = sent.map_err(|error| transport(url, &error))?;
    let status = response.status();
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after = header_seconds(&response, "retry-after")
            .or_else(|| header_seconds(&response, "x-ratelimit-reset"));
        return Err(HttpError::RateLimited {
            url: shown(url).to_owned(),
            retry_after,
        });
    }
    if !status.is_success() {
        return Err(HttpError::Status {
            url: shown(url).to_owned(),
            status: status.as_u16(),
        });
    }
    Ok(response)
}

fn read_limited(url: &str, response: Response<Body>, limit: u64) -> Result<Vec<u8>, HttpError> {
    let body = response
        .into_body()
        .into_with_config()
        .limit(reader_limit(limit))
        .read_to_vec()
        .map_err(|error| transport(url, &error))?;
    if body.len() as u64 > limit {
        return Err(too_large(url, limit));
    }
    Ok(body)
}

fn too_large(url: &str, limit: u64) -> HttpError {
    HttpError::TooLarge {
        url: shown(url).to_owned(),
        limit,
    }
}

/// `url` without its query or fragment, for an error. Signed download links carry their
/// credentials in the query, and an error message is somewhere they must not go
/// (`docs/07-browser-and-secrets.md` §7.5).
fn shown(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

/// `message` with the query and fragment removed from every URL in it.
fn without_queries(message: &str) -> String {
    let mut shown_message = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(start) = rest.find("://") {
        let (before, after) = rest.split_at(start);
        shown_message.push_str(before);
        let end = after
            .find(|character: char| {
                character.is_whitespace()
                    || matches!(
                        character,
                        '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | ','
                    )
            })
            .unwrap_or(after.len());
        let (url, tail) = after.split_at(end);
        shown_message.push_str(shown(url));
        rest = tail;
    }
    shown_message.push_str(rest);
    shown_message
}

fn header_seconds(response: &Response<Body>, name: &str) -> Option<u64> {
    response
        .headers()
        .get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn transport(url: &str, error: &ureq::Error) -> HttpError {
    match error {
        // The reader limit carries one byte of headroom; report the caller's limit.
        ureq::Error::BodyExceedsLimit(limit) => too_large(url, limit.saturating_sub(1)),
        other => HttpError::Transport {
            url: shown(url).to_owned(),
            message: without_queries(&other.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        thread,
    };

    use msbe_provider_api::{HttpClient, HttpError};

    use super::{USER_AGENT, UreqClient, without_queries};

    /// Serves one canned HTTP response on a loopback port. Returns the URL and a handle that
    /// yields the request the server received: its head, then its body.
    fn serve_once(response: &'static str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/path", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.is_empty() || line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse().unwrap();
                }
                request.push_str(&line);
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            request.push_str(&String::from_utf8(body).unwrap());
            // The client may hang up early, for example after a size limit trips.
            drop(stream.write_all(response.as_bytes()));
            request
        });
        (url, handle)
    }

    /// A client for the plain-`http` loopback servers these tests run. Real clients are
    /// https-only.
    fn loopback_client() -> UreqClient {
        UreqClient::build(false).unwrap()
    }

    #[test]
    fn plain_http_is_refused_before_any_connection_is_made() {
        let client = UreqClient::connect().expect("the system trust store has certificates");
        let result = client.get("http://127.0.0.1:9/never", &[], 64);
        assert!(
            matches!(&result, Err(HttpError::Transport { message, .. }) if message.contains("https only")),
            "{result:?}"
        );
    }

    #[test]
    fn a_json_post_sends_its_body_and_content_type() {
        let (url, server) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}");
        let client = loopback_client();
        assert_eq!(
            client.post_json(&url, br#"{"hashes":[]}"#, 64).unwrap(),
            b"{}"
        );

        let request = server.join().unwrap();
        let lowered = request.to_ascii_lowercase();
        assert!(lowered.starts_with("post /path "), "{request}");
        assert!(
            lowered.contains("content-type: application/json"),
            "{request}"
        );
        assert!(request.ends_with(r#"{"hashes":[]}"#), "{request}");
    }

    #[test]
    fn a_successful_body_is_returned_with_the_identifying_user_agent_sent() {
        let (url, server) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello");
        let client = loopback_client();
        assert_eq!(
            client.get(&url, &[("q", "sodium")], 1024).unwrap(),
            b"hello"
        );

        let head = server.join().unwrap().to_ascii_lowercase();
        assert!(head.starts_with("get /path?q=sodium "), "{head}");
        assert!(
            head.contains(&format!("user-agent: {}", USER_AGENT.to_ascii_lowercase())),
            "{head}"
        );
    }

    #[test]
    fn rate_limits_statuses_and_oversized_bodies_are_distinct_errors() {
        let client = loopback_client();

        let (url, server) = serve_once(
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 7\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let result = client.get(&url, &[], 64);
        assert!(
            matches!(
                result,
                Err(HttpError::RateLimited {
                    retry_after: Some(7),
                    ..
                })
            ),
            "{result:?}"
        );
        server.join().unwrap();

        let (url, server) =
            serve_once("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let result = client.get(&url, &[], 64);
        assert!(
            matches!(result, Err(HttpError::Status { status: 404, .. })),
            "{result:?}"
        );
        server.join().unwrap();

        let (url, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
        );
        let result = client.get(&url, &[], 4);
        assert!(
            matches!(result, Err(HttpError::TooLarge { limit: 4, .. })),
            "{result:?}"
        );
        server.join().unwrap();
    }

    /// Found against a real CDN: a file whose size equals the limit was refused.
    #[test]
    fn a_body_of_exactly_the_limit_is_accepted_and_one_byte_more_is_not() {
        let client = loopback_client();

        let (url, server) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd");
        assert_eq!(client.get(&url, &[], 4).unwrap(), b"abcd");
        server.join().unwrap();

        let (url, server) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nabcde");
        let result = client.get(&url, &[], 4);
        assert!(
            matches!(result, Err(HttpError::TooLarge { limit: 4, .. })),
            "{result:?}"
        );
        server.join().unwrap();

        let (url, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world",
        );
        let mut sink = Vec::new();
        assert_eq!(client.download(&url, &mut sink, 11).unwrap(), 11);
        server.join().unwrap();

        let (url, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nhello world!",
        );
        let result = client.download(&url, &mut Vec::new(), 11);
        assert!(
            matches!(result, Err(HttpError::TooLarge { limit: 11, .. })),
            "{result:?}"
        );
        server.join().unwrap();
    }

    #[test]
    fn errors_never_repeat_a_query_or_fragment() {
        let client = loopback_client();
        let (url, server) =
            serve_once("HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let signed = format!("{url}?key=signed-download-key&expires=1#part");
        let result = client.download(&signed, &mut Vec::new(), 64);
        assert!(
            matches!(&result, Err(HttpError::Status { url: reported, status: 403 }) if *reported == url),
            "{result:?}"
        );
        server.join().unwrap();

        let refused = UreqClient::connect()
            .expect("the system trust store has certificates")
            .get("http://127.0.0.1:9/never?key=signed-download-key", &[], 64);
        let described = format!("{refused:?}");
        assert!(!described.contains("signed-download-key"), "{described}");

        assert_eq!(
            without_queries(
                "redirected from https://a.test/x?key=one to \"https://b.test/y#key=two\", then (ftp://c.test/?z)"
            ),
            "redirected from https://a.test/x to \"https://b.test/y\", then (ftp://c.test/)"
        );
    }

    #[test]
    fn downloads_stream_into_the_sink() {
        let client = loopback_client();
        let (url, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world",
        );
        let mut sink = Vec::new();
        assert_eq!(client.download(&url, &mut sink, 1024).unwrap(), 11);
        assert_eq!(sink, b"hello world");
        server.join().unwrap();
    }

    #[test]
    #[ignore = "needs network access to index.crates.io"]
    fn a_public_host_answers_over_https_with_the_system_trust_store() {
        let client = UreqClient::connect().unwrap();
        let body = client
            .get("https://index.crates.io/config.json", &[], 1 << 20)
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("\"dl\""));
    }
}
