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
//!
//! ureq follows redirects itself, but it drops only `Authorization` and `Cookie` when one leads
//! to another host; any other header goes along. So a request carrying a credential header follows
//! its redirects here instead, within its own origin only.

use std::{fmt, io, io::Write, sync::Arc, time::Duration};

use msbe_provider_api::{
    HttpClient, HttpError, HttpRequest, HttpResponse, Method, Origin, Rate, without_query,
};
use ureq::{
    Agent, Body, RequestBuilder,
    http::{Response, StatusCode, header},
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

/// The most redirects one request follows, as ureq's default.
const REDIRECT_LIMIT: usize = 10;

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

    /// Sends `request` and returns its final, successful response.
    ///
    /// A request without a credential lets ureq follow redirects. One with a credential follows
    /// them here, and refuses any that leaves the origin it was sent to.
    fn call(&self, request: &HttpRequest<'_>) -> Result<Response<Body>, HttpError> {
        if !request.headers.iter().any(|header| header.credential) {
            return checked(
                request.url,
                self.once(request, request.method, request.url, true),
            );
        }
        let origin = Origin::of(request.url);
        let mut method = request.method;
        let mut url = request.url.to_owned();
        for hop in 0..=REDIRECT_LIMIT {
            let response = self
                .once(request, method, &url, false)
                .map_err(|error| transport(&url, &error))?;
            let status = response.status();
            let location = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok());
            let (true, Some(location)) = (is_followed(status), location) else {
                return checked(&url, Ok(response));
            };
            let next = resolve(&url, location).ok_or_else(|| HttpError::Transport {
                url: without_query(&url).to_owned(),
                message: "redirected to a location that is not a URL".to_owned(),
            })?;
            if !origin.as_ref().is_some_and(|origin| origin.serves(&next)) {
                return Err(HttpError::CrossOriginRedirect {
                    url: without_query(&url).to_owned(),
                    location: without_query(&next).to_owned(),
                });
            }
            if hop == REDIRECT_LIMIT {
                break;
            }
            if !matches!(
                status,
                StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
            ) {
                method = Method::Get;
            }
            url = next;
        }
        Err(HttpError::Transport {
            url: without_query(request.url).to_owned(),
            message: "too many redirects".to_owned(),
        })
    }

    /// Sends one request to `url` with `request`'s headers, following redirects only when
    /// `follow` is set. The query goes only to the URL the request was made for; a redirect's
    /// location carries its own.
    fn once(
        &self,
        request: &HttpRequest<'_>,
        method: Method<'_>,
        url: &str,
        follow: bool,
    ) -> Result<Response<Body>, ureq::Error> {
        let query = if url == request.url {
            request.query
        } else {
            &[]
        };
        match method {
            Method::Get => prepared(self.agent.get(url), request, query, follow).call(),
            Method::Post { json } => prepared(self.agent.post(url), request, query, follow)
                .content_type("application/json")
                .send(json),
        }
    }
}

impl HttpClient for UreqClient {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
        let response = self.call(request)?;
        let rate = rate(&response, request.quota_headers);
        let body = read_limited(request.url, response, request.limit)?;
        Ok(HttpResponse { body, rate })
    }

    fn download(&self, request: &HttpRequest<'_>, sink: &mut dyn Write) -> Result<u64, HttpError> {
        let (url, limit) = (request.url, request.limit);
        let mut reader = self
            .call(request)?
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

/// `builder` with `request`'s headers and `query`, following redirects only when `follow` is set.
fn prepared<B>(
    builder: RequestBuilder<B>,
    request: &HttpRequest<'_>,
    query: &[(&str, &str)],
    follow: bool,
) -> RequestBuilder<B> {
    let builder = request.headers.iter().fold(
        builder.query_pairs(query.iter().copied()),
        |builder, header| builder.header(header.name, header.value),
    );
    if follow {
        builder
    } else {
        builder.config().max_redirects(0).build()
    }
}

/// Whether `status` is a redirect with a location to follow.
fn is_followed(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

/// The absolute URL a redirect's `location` names, relative to `base`.
fn resolve(base: &str, location: &str) -> Option<String> {
    if location.is_empty()
        || location
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return None;
    }
    let (scheme, rest) = base.split_once("://")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, _) = rest.split_at(authority_end);
    let base = without_query(base);
    if has_scheme(location) {
        Some(location.to_owned())
    } else if location.starts_with("//") {
        Some(format!("{scheme}:{location}"))
    } else if location.starts_with('/') {
        Some(format!("{scheme}://{authority}{location}"))
    } else if location.starts_with(['?', '#']) {
        // A fragment is never sent, so a fragment-only location is the base again.
        Some(match location.strip_prefix('?') {
            Some(query) => format!("{base}?{query}"),
            None => base.to_owned(),
        })
    } else {
        let directory = base
            .rfind('/')
            .filter(|slash| *slash >= scheme.len() + "://".len() + authority.len())
            .map_or_else(
                || format!("{base}/"),
                |slash| base.split_at(slash + 1).0.to_owned(),
            );
        Some(format!("{directory}{location}"))
    }
}

/// Whether `reference` starts with a URI scheme, `letter *( letter / digit / "+" / "-" / "." ) ":"`.
fn has_scheme(reference: &str) -> bool {
    reference.split_once(':').is_some_and(|(scheme, _)| {
        let mut bytes = scheme.bytes();
        bytes
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    })
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
        let retry_after = header_number(&response, "retry-after")
            .or_else(|| header_number(&response, "x-ratelimit-reset"));
        return Err(HttpError::RateLimited {
            url: without_query(url).to_owned(),
            retry_after,
        });
    }
    if !status.is_success() {
        return Err(HttpError::Status {
            url: without_query(url).to_owned(),
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
        url: without_query(url).to_owned(),
        limit,
    }
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
        shown_message.push_str(without_query(url));
        rest = tail;
    }
    shown_message.push_str(rest);
    shown_message
}

/// The remaining quota `response` reports in the headers `names` lists.
fn rate(response: &Response<Body>, names: &[String]) -> Rate {
    Rate {
        remaining: names
            .iter()
            .filter_map(|name| Some((name.to_ascii_lowercase(), header_number(response, name)?)))
            .collect(),
    }
}

fn header_number(response: &Response<Body>, name: &str) -> Option<u64> {
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
            url: without_query(url).to_owned(),
            message: without_queries(&other.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::{TcpListener, TcpStream},
        thread,
    };

    use msbe_provider_api::{Header, HttpClient, HttpError, HttpRequest};

    use super::{USER_AGENT, UreqClient, resolve, without_queries};

    /// A loopback listener and its base URL, `http://<address>`.
    fn listen() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        (listener, base)
    }

    /// Answers one connection on `listener` with each of `responses` in turn. The handle yields
    /// each request the server received: its head, then its body.
    fn serve(listener: TcpListener, responses: Vec<String>) -> thread::JoinHandle<Vec<String>> {
        thread::spawn(move || {
            responses
                .into_iter()
                .map(|response| {
                    let (mut stream, _) = listener.accept().unwrap();
                    let request = read_request(&stream);
                    // The client may hang up early, for example after a size limit trips.
                    drop(stream.write_all(response.as_bytes()));
                    request
                })
                .collect()
        })
    }

    fn read_request(stream: &TcpStream) -> String {
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
        request
    }

    /// Serves one canned HTTP response on a loopback port. Returns the URL and a handle that
    /// yields the request the server received.
    fn serve_once(response: &str) -> (String, thread::JoinHandle<String>) {
        let (listener, base) = listen();
        let server = serve(listener, vec![response.to_owned()]);
        (
            format!("{base}/path"),
            thread::spawn(move || server.join().unwrap().concat()),
        )
    }

    fn redirect(status: &str, location: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
    }

    /// A client for the plain-`http` loopback servers these tests run. Real clients are
    /// https-only.
    fn loopback_client() -> UreqClient {
        UreqClient::build(false).unwrap()
    }

    #[test]
    fn plain_http_is_refused_before_any_connection_is_made() {
        let client = UreqClient::connect().expect("the system trust store has certificates");
        let result = client.send(&HttpRequest::get("http://127.0.0.1:9/never", 64));
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
            client
                .send(&HttpRequest::post_json(&url, br#"{"hashes":[]}"#, 64))
                .unwrap()
                .body,
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
        let request = HttpRequest::get(&url, 1024)
            .with_query(&[("q", "sodium")])
            .with_header(Header::new("accept", "text/plain"));
        assert_eq!(client.send(&request).unwrap().body, b"hello");

        let head = server.join().unwrap().to_ascii_lowercase();
        assert!(head.starts_with("get /path?q=sodium "), "{head}");
        assert!(head.contains("accept: text/plain"), "{head}");
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
        let result = client.send(&HttpRequest::get(&url, 64));
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
        let result = client.send(&HttpRequest::get(&url, 64));
        assert!(
            matches!(result, Err(HttpError::Status { status: 404, .. })),
            "{result:?}"
        );
        server.join().unwrap();

        let (url, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
        );
        let result = client.send(&HttpRequest::get(&url, 4));
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
        assert_eq!(
            client.send(&HttpRequest::get(&url, 4)).unwrap().body,
            b"abcd"
        );
        server.join().unwrap();

        let (url, server) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nabcde");
        let result = client.send(&HttpRequest::get(&url, 4));
        assert!(
            matches!(result, Err(HttpError::TooLarge { limit: 4, .. })),
            "{result:?}"
        );
        server.join().unwrap();

        let (url, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world",
        );
        let mut sink = Vec::new();
        assert_eq!(
            client
                .download(&HttpRequest::get(&url, 11), &mut sink)
                .unwrap(),
            11
        );
        server.join().unwrap();

        let (url, server) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nhello world!",
        );
        let result = client.download(&HttpRequest::get(&url, 11), &mut Vec::new());
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
        let result = client.download(&HttpRequest::get(&signed, 64), &mut Vec::new());
        assert!(
            matches!(&result, Err(HttpError::Status { url: reported, status: 403 }) if *reported == url),
            "{result:?}"
        );
        server.join().unwrap();

        let refused = UreqClient::connect()
            .expect("the system trust store has certificates")
            .send(&HttpRequest::get(
                "http://127.0.0.1:9/never?key=signed-download-key",
                64,
            ));
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
        assert_eq!(
            client
                .download(&HttpRequest::get(&url, 1024), &mut sink)
                .unwrap(),
            11
        );
        assert_eq!(sink, b"hello world");
        server.join().unwrap();
    }

    /// Why credentialed requests follow redirects themselves: ureq 3 strips only `Authorization`
    /// and `Cookie` on a redirect, so any other header reaches whatever origin the server names.
    /// If this starts failing, ureq changed that rule.
    #[test]
    fn ureq_itself_carries_a_custom_header_to_another_origin() {
        let (second, second_base) = listen();
        let landing = serve(
            second,
            vec![
                "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nlanded"
                    .to_owned(),
            ],
        );
        let (first, first_base) = listen();
        let redirecting = serve(
            first,
            vec![redirect("302 Found", &format!("{second_base}/landing"))],
        );

        let url = format!("{first_base}/start");
        let request = HttpRequest::get(&url, 64).with_header(Header::new("x-probe", "carried"));
        assert_eq!(loopback_client().send(&request).unwrap().body, b"landed");

        redirecting.join().unwrap();
        let landed = landing.join().unwrap().concat().to_ascii_lowercase();
        assert!(landed.starts_with("get /landing "), "{landed}");
        assert!(landed.contains("x-probe: carried"), "{landed}");
    }

    #[test]
    fn a_credential_is_never_carried_to_another_origin_by_a_redirect() {
        let client = loopback_client();
        for download in [false, true] {
            let (second, second_base) = listen();
            let other_origin = serve(
                second,
                vec![
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                ],
            );
            let (first, first_base) = listen();
            let redirecting = serve(
                first,
                vec![redirect(
                    "307 Temporary Redirect",
                    &format!("{second_base}/collect?token=location-query-secret"),
                )],
            );

            let url = format!("{first_base}/v1/validate");
            let request = HttpRequest::get(&url, 64)
                .with_query(&[("key", "request-query-secret")])
                .with_header(Header::credential("x-api-key", "credential-value-1"));
            let result = if download {
                client.download(&request, &mut Vec::new()).map(|_| ())
            } else {
                client.send(&request).map(|_| ())
            };
            let Err(error @ HttpError::CrossOriginRedirect { .. }) = result else {
                panic!("{result:?}");
            };
            for shown in [format!("{error}"), format!("{error:?}")] {
                for secret in [
                    "credential-value-1",
                    "request-query-secret",
                    "location-query-secret",
                ] {
                    assert!(!shown.contains(secret), "{shown}");
                }
                assert!(shown.contains(&format!("{second_base}/collect")), "{shown}");
            }
            let sent = redirecting.join().unwrap().concat().to_ascii_lowercase();
            assert!(sent.contains("x-api-key: credential-value-1"), "{sent}");

            // Had the client followed, its request would be the one the other origin received.
            let mut marker = TcpStream::connect(second_base.trim_start_matches("http://")).unwrap();
            marker.write_all(b"GET /marker HTTP/1.1\r\n\r\n").unwrap();
            let received = other_origin.join().unwrap().concat();
            assert!(received.starts_with("GET /marker "), "{received}");
        }
    }

    #[test]
    fn a_credential_follows_redirects_within_its_origin_and_quota_headers_are_read() {
        let (listener, base) = listen();
        let server = serve(
            listener,
            vec![
                redirect("303 See Other", "/v1/moved?page=2"),
                redirect("302 Found", &format!("{base}/v1/final")),
                "HTTP/1.1 200 OK\r\nX-RL-Hourly-Remaining: 99\r\nx-rl-daily-remaining: 2500\r\nx-rl-reset: soon\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                    .to_owned(),
            ],
        );
        let url = format!("{base}/v1/lookup");
        let quota = [
            "x-rl-hourly-remaining".to_owned(),
            "x-rl-daily-remaining".to_owned(),
            "x-rl-reset".to_owned(),
            "x-rl-absent".to_owned(),
        ];
        let mut request = HttpRequest::post_json(&url, b"{\"ids\":[1]}", 64)
            .with_query(&[("game", "one")])
            .with_header(Header::credential("x-api-key", "credential-value-2"));
        request.quota_headers = &quota;
        let response = loopback_client().send(&request).unwrap();
        assert_eq!(response.body, b"{}");
        assert_eq!(
            response.rate.remaining.into_iter().collect::<Vec<_>>(),
            [
                ("x-rl-daily-remaining".to_owned(), 2500),
                ("x-rl-hourly-remaining".to_owned(), 99),
            ]
        );

        let requests = server.join().unwrap();
        let heads: Vec<String> = requests
            .iter()
            .map(|request| request.lines().next().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(
            heads,
            [
                "POST /v1/lookup?game=one HTTP/1.1",
                "GET /v1/moved?page=2 HTTP/1.1",
                "GET /v1/final HTTP/1.1",
            ]
        );
        for request in &requests {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("x-api-key: credential-value-2"),
                "{request}"
            );
        }
    }

    #[test]
    fn redirect_locations_resolve_against_the_redirected_url() {
        let base = "https://api.example.test/v1/games/files.json?key=1";
        for (location, expected) in [
            ("https://cdn.example.test/a", "https://cdn.example.test/a"),
            ("//cdn.example.test/a", "https://cdn.example.test/a"),
            ("/v2/files", "https://api.example.test/v2/files"),
            (
                "/next?url=https://other.test/",
                "https://api.example.test/next?url=https://other.test/",
            ),
            (
                "?page=2",
                "https://api.example.test/v1/games/files.json?page=2",
            ),
            ("#part", "https://api.example.test/v1/games/files.json"),
            ("other.json", "https://api.example.test/v1/games/other.json"),
            ("a:b/c", "a:b/c"),
        ] {
            assert_eq!(
                resolve(base, location).as_deref(),
                Some(expected),
                "{location}"
            );
        }
        assert_eq!(
            resolve("https://api.example.test", "files").as_deref(),
            Some("https://api.example.test/files")
        );
        for refused in ["", "/a b", "/a\tb"] {
            assert_eq!(resolve(base, refused), None, "{refused:?}");
        }
    }

    #[test]
    #[ignore = "needs network access to index.crates.io"]
    fn a_public_host_answers_over_https_with_the_system_trust_store() {
        let client = UreqClient::connect().unwrap();
        let body = client
            .send(&HttpRequest::get(
                "https://index.crates.io/config.json",
                1 << 20,
            ))
            .unwrap()
            .body;
        assert!(String::from_utf8_lossy(&body).contains("\"dl\""));
    }
}
