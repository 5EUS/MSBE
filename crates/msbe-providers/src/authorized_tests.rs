//! The client each registered adapter is handed: its credential only on requests for its metadata
//! origin, never on downloads, and the quota those responses report recorded.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use msbe_core::instance::NativeExtensionIdentity;
use msbe_plan_schema::Side;
use msbe_provider_api::{
    Adapter, AdapterError, ApiHeaders, Build, EndpointError, Header, HeaderError, HttpClient,
    HttpError, HttpRequest, HttpResponse, ManifestError, Method, Provider, Rate, Registration,
    Search, Target,
    model::{Download, ReleaseFile, Request, SearchResult},
};
use msbe_secrets::{Secret, StoreError};

use crate::{CredentialSource, Providers, RegistryError};

const MANIFEST: &str = r#"
    schema = 1
    id = "keyed"
    name = "Keyed"
    [source]
    type = "prefixed"
    prefix = "keyed:"
    [metadata]
    api_base = "https://api.keyed.test/v1"
    [acquisition]
    type = "direct_https"
    [policy]
    requires_auth = false
    respects_distribution_flag = false
    tos_url = ""
    ack_required = false
"#;

/// Every URL the adapter's search requests, and whether it has the metadata origin.
const REQUESTS: &[(&str, bool)] = &[
    ("https://api.keyed.test/v1/search", true),
    ("HTTPS://API.KEYED.TEST:443/v2/other", true),
    ("https://api.keyed.test:8443/v1/search", false),
    ("http://api.keyed.test/v1/search", false),
    ("https://cdn.keyed.test/v1/search", false),
    ("https://api.keyed.test.other.test/v1/search", false),
    ("https://api.keyed.test@other.test/v1/search", false),
];

const LOOKUP: &str = "https://api.keyed.test/v1/lookup";
const ARTIFACT: &str = "https://api.keyed.test/v1/files/mod.zip";

/// An adapter that names `credential` as its credential header and requests [`REQUESTS`].
#[derive(Debug)]
struct Keyed {
    credential: Option<&'static str>,
}

impl Adapter for Keyed {
    fn id(&self) -> &'static str {
        "keyed"
    }

    fn request(&self, reference: &str) -> Result<Request, AdapterError> {
        Ok(Request::Project {
            reference: reference.to_owned(),
            version: None,
        })
    }

    fn as_search(&self) -> Option<&dyn Search> {
        Some(self)
    }

    fn api_headers(&self) -> ApiHeaders {
        ApiHeaders {
            credential: self.credential.map(str::to_owned),
            quota: vec!["x-remaining".to_owned()],
        }
    }
}

impl Search for Keyed {
    /// Gets every URL in [`REQUESTS`], the first with a header of its own named like the
    /// credential header, then posts to [`LOOKUP`].
    fn search(
        &self,
        http: &dyn HttpClient,
        _: &str,
        _: &Target,
        _: u8,
    ) -> Result<Vec<SearchResult>, AdapterError> {
        for (index, (url, _)) in REQUESTS.iter().enumerate() {
            let mut request =
                HttpRequest::get(url, 64).with_header(Header::new("accept", "application/json"));
            if index == 0 {
                request = request.with_header(Header::new("X-Api-Key", "spoofed-by-adapter"));
            }
            http.send(&request).map_err(EndpointError::from)?;
        }
        http.send(&HttpRequest::post_json(LOOKUP, b"{}", 64))
            .map_err(EndpointError::from)?;
        Ok(Vec::new())
    }
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "test builders must implement the fallible registration function pointer"
)]
fn keyed(_: &Provider) -> Result<Box<dyn Adapter>, ManifestError> {
    Ok(Box::new(Keyed {
        credential: Some("x-api-key"),
    }))
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "test builders must implement the fallible registration function pointer"
)]
fn keyless(_: &Provider) -> Result<Box<dyn Adapter>, ManifestError> {
    Ok(Box::new(Keyed { credential: None }))
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "test builders must implement the fallible registration function pointer"
)]
fn reserved(_: &Provider) -> Result<Box<dyn Adapter>, ManifestError> {
    Ok(Box::new(Keyed {
        credential: Some("authorization"),
    }))
}

fn registration(build: Build) -> Registration {
    Registration {
        id: "keyed",
        identity: NativeExtensionIdentity {
            id: "keyed",
            version: "1.0.0",
            host_api_minimum: 1,
            host_api_maximum: 1,
            signer: "msbe-build",
        },
        manifest: MANIFEST,
        overlay: &[],
        build,
        pack_codecs: &[],
        wasm_pack_codecs: &[],
        exception_reason: "Test adapter.",
    }
}

/// A credential store holding `value`, or failing, that counts how often it is asked.
#[derive(Debug, Default)]
struct Stored {
    value: Option<&'static str>,
    fails: bool,
    lookups: AtomicUsize,
}

impl Stored {
    fn holding(value: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            value,
            ..Self::default()
        })
    }

    fn lookups(&self) -> usize {
        self.lookups.load(Ordering::SeqCst)
    }
}

impl CredentialSource for Stored {
    fn credential(&self, provider: &str) -> Result<Option<Secret>, StoreError> {
        assert_eq!(provider, "keyed");
        self.lookups.fetch_add(1, Ordering::SeqCst);
        if self.fails {
            return Err(StoreError::Locked);
        }
        Ok(self
            .value
            .map(|value| Secret::new(value.to_owned()).unwrap()))
    }
}

/// Records each request with its headers, a credential marked `!`, and the quota headers it
/// asks for, and reports 7 remaining for each of those.
#[derive(Default)]
struct Recorder {
    log: RefCell<Vec<String>>,
}

impl Recorder {
    fn entries(&self) -> Vec<String> {
        self.log.borrow().clone()
    }
}

impl HttpClient for Recorder {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
        let method = match request.method {
            Method::Get => "GET",
            Method::Post { .. } => "POST",
        };
        self.log
            .borrow_mut()
            .push(format!("{method} {} {}", request.url, rendered(request)));
        Ok(HttpResponse {
            body: Vec::new(),
            rate: Rate {
                remaining: request
                    .quota_headers
                    .iter()
                    .map(|name| (name.clone(), 7))
                    .collect(),
            },
        })
    }

    fn download(&self, request: &HttpRequest<'_>, sink: &mut dyn Write) -> Result<u64, HttpError> {
        self.log
            .borrow_mut()
            .push(format!("DOWNLOAD {} {}", request.url, rendered(request)));
        sink.write_all(b"bytes").unwrap();
        Ok(5)
    }
}

fn rendered(request: &HttpRequest<'_>) -> String {
    let headers: Vec<String> = request
        .headers
        .iter()
        .map(|header| {
            let mark = if header.credential { "!" } else { "" };
            format!("{mark}{}={}", header.name, header.value)
        })
        .collect();
    format!("[{}] quota={:?}", headers.join(" "), request.quota_headers)
}

fn target() -> Target {
    Target {
        game: "game".to_owned(),
        edition: None,
        storefront: None,
        loader: "loader".to_owned(),
        provides: Vec::new(),
        loader_version: None,
        game_version: None,
        side: Side::Client,
    }
}

/// Searches, then acquires [`ARTIFACT`], through `providers`, and returns what `http` was sent.
fn search_and_acquire(providers: &Providers) -> Result<Vec<String>, RegistryError> {
    let http = Recorder::default();
    providers.search("keyed", &http, "query", &target(), 10)?;
    let file = ReleaseFile {
        download: Download::Direct {
            url: ARTIFACT.to_owned(),
        },
        name: "mod.zip".to_owned(),
        size: None,
        md5: None,
        sha1: None,
        sha256: None,
        sha512: None,
        primary: true,
    };
    let dir = tempfile::tempdir().unwrap();
    providers
        .adapter("keyed")?
        .acquire(&http, &file, dir.path())?;
    Ok(http.entries())
}

/// What [`search_and_acquire`] sends when requests for the metadata origin carry `origin_headers`.
fn expected(origin_headers: &[&str], first_extra: Option<&str>) -> Vec<String> {
    let quota = "quota=[\"x-remaining\"]";
    let mut entries: Vec<String> = REQUESTS
        .iter()
        .enumerate()
        .map(|(index, (url, on_origin))| {
            let mut headers = vec!["accept=application/json"];
            if index == 0
                && let Some(extra) = first_extra
            {
                headers.push(extra);
            }
            if *on_origin {
                headers.extend_from_slice(origin_headers);
                format!("GET {url} [{}] {quota}", headers.join(" "))
            } else {
                format!("GET {url} [{}] quota=[]", headers.join(" "))
            }
        })
        .collect();
    entries.push(format!(
        "POST {LOOKUP} [{}] {quota}",
        origin_headers.join(" ")
    ));
    entries.push(format!("DOWNLOAD {ARTIFACT} [] quota=[]"));
    entries
}

#[test]
fn a_credential_goes_only_to_the_metadata_origin_and_never_to_a_download()
-> Result<(), RegistryError> {
    let source = Stored::holding(Some("credential-value-7"));
    let providers = Providers::new(&[registration(keyed)], &[])?
        .with_credentials(Arc::<Stored>::clone(&source));
    assert_eq!(providers.rate("keyed"), None);

    assert_eq!(
        search_and_acquire(&providers)?,
        expected(&["!x-api-key=credential-value-7"], None),
        "the adapter's own x-api-key header is dropped, and only the origin's requests carry one"
    );
    assert_eq!(source.lookups(), 1, "the store is asked once per registry");
    assert_eq!(
        providers.rate("keyed"),
        Some(Rate {
            remaining: BTreeMap::from([("x-remaining".to_owned(), 7)]),
        })
    );
    assert!(!format!("{providers:?}").contains("credential-value-7"));
    Ok(())
}

#[test]
fn without_a_stored_credential_nothing_is_attached_and_the_store_is_asked_once()
-> Result<(), RegistryError> {
    let source = Stored::holding(None);
    let providers = Providers::new(&[registration(keyed)], &[])?
        .with_credentials(Arc::<Stored>::clone(&source));
    assert_eq!(search_and_acquire(&providers)?, expected(&[], None));
    assert_eq!(source.lookups(), 1);

    let unsourced = Providers::new(&[registration(keyed)], &[])?;
    assert_eq!(search_and_acquire(&unsourced)?, expected(&[], None));
    Ok(())
}

#[test]
fn an_adapter_that_names_no_credential_header_is_never_given_one() -> Result<(), RegistryError> {
    let source = Stored::holding(Some("credential-value-8"));
    let providers = Providers::new(&[registration(keyless)], &[])?
        .with_credentials(Arc::<Stored>::clone(&source));
    assert_eq!(
        search_and_acquire(&providers)?,
        expected(&[], Some("X-Api-Key=spoofed-by-adapter"))
    );
    assert_eq!(source.lookups(), 0);
    Ok(())
}

#[test]
fn a_credential_that_cannot_be_read_fails_the_request_before_it_is_sent()
-> Result<(), RegistryError> {
    let source = Arc::new(Stored {
        fails: true,
        ..Stored::default()
    });
    let providers = Providers::new(&[registration(keyed)], &[])?.with_credentials(source);
    let http = Recorder::default();
    let result = providers.search("keyed", &http, "query", &target(), 10);
    let Err(RegistryError::Adapter(AdapterError::Endpoint(EndpointError::Http(
        HttpError::Credential { url, reason },
    )))) = result
    else {
        panic!("{result:?}");
    };
    assert_eq!(url, "https://api.keyed.test/v1/search");
    assert!(reason.contains("locked"), "{reason}");
    assert!(http.entries().is_empty());
    Ok(())
}

#[test]
fn unusable_credential_headers_are_refused_at_registration() {
    let result = Providers::new(&[registration(reserved)], &[]);
    assert!(
        matches!(
            &result,
            Err(RegistryError::ApiHeaders {
                provider,
                source: HeaderError::Reserved(header),
            }) if provider == "keyed" && header == "authorization"
        ),
        "{result:?}"
    );

    let without_metadata: &'static str = MANIFEST
        .replace("api_base = \"https://api.keyed.test/v1\"", "")
        .replace("[metadata]", "")
        .leak();
    let result = Providers::new(
        &[Registration {
            manifest: without_metadata,
            ..registration(keyed)
        }],
        &[],
    );
    assert!(
        matches!(
            &result,
            Err(RegistryError::Manifest(ManifestError::MissingMetadata(id))) if id == "keyed"
        ),
        "{result:?}"
    );
    assert!(
        Providers::new(
            &[Registration {
                manifest: without_metadata,
                ..registration(keyless)
            }],
            &[],
        )
        .is_ok()
    );
}
