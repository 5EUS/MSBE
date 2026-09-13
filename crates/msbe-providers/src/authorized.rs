//! Each provider's requests, with its credential attached only where it may go and the quota its
//! API reports recorded (`docs/07-browser-and-secrets.md` §7.5).
//!
//! The registry wraps every adapter it builds, so no caller can hand an adapter a client that
//! skips these rules. An adapter names the header its credential goes in and never holds the
//! credential. The credential is attached only to requests for the origin of the provider's
//! `api_base`, never to an artifact download, and the transport refuses a redirect that would
//! carry it to another origin.

use std::{
    collections::BTreeMap,
    fmt,
    io::Write,
    path::Path,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use msbe_provider_api::{
    Accounts, AcquiredArtifact, Adapter, AdapterError, ApiHeaders, Handoff, Header, HttpClient,
    HttpError, HttpRequest, HttpResponse, ManifestError, Origin, PackageId, Provenance, Provider,
    Rate, Releases, Search, Target, UpdateCheck, Updates,
    model::{Account, HandoffTicket, Project, Release, ReleaseFile, Request, SearchResult},
    without_query,
};
use msbe_secrets::{Credentials, Secret, StoreError};
use thiserror::Error;

use crate::RegistryError;

/// The headers a provider that requires identification is sent on requests to its API.
const IDENTITY: [(&str, &str); 2] = [
    ("application-name", "MSBE"),
    ("application-version", env!("CARGO_PKG_VERSION")),
];

/// Where the registry finds a provider's credential when a request needs one.
pub trait CredentialSource: fmt::Debug + Send + Sync {
    /// `provider`'s credential, if it has one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the store holding it cannot be read.
    fn credential(&self, provider: &str) -> Result<Option<Secret>, StoreError>;
}

impl CredentialSource for Mutex<Credentials> {
    fn credential(&self, provider: &str) -> Result<Option<Secret>, StoreError> {
        lock(self).get(provider)
    }
}

/// Each provider's credential as read from the source, `None` when it has none.
type KnownSecrets = BTreeMap<String, Option<Arc<Secret>>>;

/// The credential source, the credentials read from it, and the quotas reported, shared by every
/// adapter one registry wraps.
#[derive(Default)]
pub(crate) struct Keys {
    source: Mutex<Option<Arc<dyn CredentialSource>>>,
    /// Credentials already read, so a keyring is asked at most once per provider.
    secrets: Mutex<KnownSecrets>,
    rates: Mutex<BTreeMap<String, Rate>>,
}

impl Keys {
    /// Reads credentials from `source` from now on.
    pub(crate) fn set_source(&self, source: Arc<dyn CredentialSource>) {
        *lock(&self.source) = Some(source);
        lock(&self.secrets).clear();
    }

    /// The quota `provider` last reported.
    pub(crate) fn rate(&self, provider: &str) -> Option<Rate> {
        lock(&self.rates).get(provider).cloned()
    }

    fn credential(&self, provider: &str) -> Result<Option<Arc<Secret>>, StoreError> {
        if let Some(known) = lock(&self.secrets).get(provider) {
            return Ok(known.clone());
        }
        let source = lock(&self.source).clone();
        let found = source
            .map(|source| source.credential(provider))
            .transpose()?
            .flatten()
            .map(Arc::new);
        lock(&self.secrets).insert(provider.to_owned(), found.clone());
        Ok(found)
    }

    fn record(&self, provider: &str, rate: &Rate) {
        if !rate.remaining.is_empty() {
            lock(&self.rates).insert(provider.to_owned(), rate.clone());
        }
    }
}

impl fmt::Debug for Keys {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Keys")
            // Only whether there is one: a source's own `Debug` might show what it holds.
            .field("source", &lock(&self.source).is_some())
            .field("rates", &*lock(&self.rates))
            .finish_non_exhaustive()
    }
}

/// A registered adapter, every request of which goes through its provider's client.
///
/// Every [`Adapter`] method is forwarded. One added to the trait must be forwarded here too, or
/// the wrapped adapter's own implementation is silently replaced by the default.
#[derive(Debug)]
pub(crate) struct Authorized {
    adapter: Box<dyn Adapter>,
    scope: Scope,
}

/// What one provider's client attaches, and where.
#[derive(Debug)]
struct Scope {
    provider: String,
    /// The origin of the provider's `api_base`, the only one its credential is sent to.
    origin: Option<Origin>,
    headers: ApiHeaders,
    /// Whether its API requires [`IDENTITY`].
    identify: bool,
    keys: Arc<Keys>,
}

impl Authorized {
    /// Wraps `provider`'s `adapter`, once the headers it declares are usable.
    pub(crate) fn new(
        provider: &Provider,
        adapter: Box<dyn Adapter>,
        keys: Arc<Keys>,
    ) -> Result<Self, RegistryError> {
        let headers = adapter.api_headers();
        headers
            .validate()
            .map_err(|source| RegistryError::ApiHeaders {
                provider: provider.id.clone(),
                source,
            })?;
        let origin = provider.api_base().and_then(Origin::of);
        if headers.credential.is_some() && origin.is_none() {
            return Err(ManifestError::MissingMetadata(provider.id.clone()).into());
        }
        Ok(Self {
            adapter,
            scope: Scope {
                provider: provider.id.clone(),
                origin,
                headers,
                identify: provider.identify().is_some(),
                keys,
            },
        })
    }

    fn http<'a>(&'a self, http: &'a dyn HttpClient) -> AuthorizedHttp<'a> {
        AuthorizedHttp {
            inner: http,
            scope: &self.scope,
            candidate: None,
        }
    }

    /// The account `candidate` belongs to, checked with `candidate` in place of any stored
    /// credential, so a credential can be checked before it is kept.
    pub(crate) fn account_with(
        &self,
        http: &dyn HttpClient,
        candidate: &Secret,
    ) -> Result<Account, AdapterError> {
        let client = AuthorizedHttp {
            inner: http,
            scope: &self.scope,
            candidate: Some(candidate),
        };
        self.adapter
            .as_accounts()
            .ok_or_else(|| withdrawn("accounts"))?
            .account(&client)
    }
}

impl Adapter for Authorized {
    fn id(&self) -> &str {
        self.adapter.id()
    }

    fn request(&self, reference: &str) -> Result<Request, AdapterError> {
        self.adapter.request(reference)
    }

    fn as_search(&self) -> Option<&dyn Search> {
        self.adapter.as_search().is_some().then_some(self)
    }

    fn as_releases(&self) -> Option<&dyn Releases> {
        self.adapter.as_releases().is_some().then_some(self)
    }

    fn as_updates(&self) -> Option<&dyn Updates> {
        self.adapter.as_updates().is_some().then_some(self)
    }

    fn as_handoff(&self) -> Option<&dyn Handoff> {
        self.adapter.as_handoff().is_some().then_some(self)
    }

    fn as_accounts(&self) -> Option<&dyn Accounts> {
        self.adapter.as_accounts().is_some().then_some(self)
    }

    fn api_headers(&self) -> ApiHeaders {
        self.scope.headers.clone()
    }

    fn acquire(
        &self,
        http: &dyn HttpClient,
        file: &ReleaseFile,
        dir: &Path,
    ) -> Result<AcquiredArtifact, AdapterError> {
        self.adapter.acquire(&self.http(http), file, dir)
    }

    fn provenance(&self, release: &Release, acquired: &AcquiredArtifact) -> Provenance {
        self.adapter.provenance(release, acquired)
    }
}

impl Search for Authorized {
    fn search(
        &self,
        http: &dyn HttpClient,
        query: &str,
        target: &Target,
        limit: u8,
    ) -> Result<Vec<SearchResult>, AdapterError> {
        self.adapter
            .as_search()
            .ok_or_else(|| withdrawn("search"))?
            .search(&self.http(http), query, target, limit)
    }
}

impl Releases for Authorized {
    fn project(
        &self,
        http: &dyn HttpClient,
        reference: &str,
        target: &Target,
    ) -> Result<Project, AdapterError> {
        self.wrapped_releases()?
            .project(&self.http(http), reference, target)
    }

    fn releases(
        &self,
        http: &dyn HttpClient,
        project: &str,
        target: &Target,
    ) -> Result<Vec<Release>, AdapterError> {
        self.wrapped_releases()?
            .releases(&self.http(http), project, target)
    }

    fn release_project(
        &self,
        http: &dyn HttpClient,
        release: &str,
        target: &Target,
    ) -> Result<PackageId, AdapterError> {
        self.wrapped_releases()?
            .release_project(&self.http(http), release, target)
    }
}

impl Updates for Authorized {
    fn check(
        &self,
        http: &dyn HttpClient,
        installed: &[&Provenance],
        target: &Target,
    ) -> Result<Vec<UpdateCheck>, AdapterError> {
        self.adapter
            .as_updates()
            .ok_or_else(|| withdrawn("updates"))?
            .check(&self.http(http), installed, target)
    }
}

impl Authorized {
    fn wrapped_releases(&self) -> Result<&dyn Releases, AdapterError> {
        self.adapter
            .as_releases()
            .ok_or_else(|| withdrawn("releases"))
    }
}

impl Handoff for Authorized {
    fn scheme(&self) -> &str {
        self.adapter
            .as_handoff()
            .map_or("", |handoff| handoff.scheme())
    }

    fn parse(&self, uri: &str, now: u64) -> Result<HandoffTicket, AdapterError> {
        self.adapter
            .as_handoff()
            .ok_or_else(|| withdrawn("handoff"))?
            .parse(uri, now)
    }

    fn redeem(
        &self,
        http: &dyn HttpClient,
        ticket: &HandoffTicket,
    ) -> Result<ReleaseFile, AdapterError> {
        self.adapter
            .as_handoff()
            .ok_or_else(|| withdrawn("handoff"))?
            .redeem(&self.http(http), ticket)
    }
}

impl Accounts for Authorized {
    fn account(&self, http: &dyn HttpClient) -> Result<Account, AdapterError> {
        self.adapter
            .as_accounts()
            .ok_or_else(|| withdrawn("accounts"))?
            .account(&self.http(http))
    }
}

/// The wrapped adapter stopped offering a capability it offered when asked.
#[derive(Debug, Error)]
#[error("the adapter no longer offers {0}")]
struct Withdrawn(&'static str);

fn withdrawn(capability: &'static str) -> AdapterError {
    AdapterError::specific(Withdrawn(capability))
}

/// A provider's view of the network: its credential and quota headers on requests for its
/// metadata origin, and nothing added to any other request.
struct AuthorizedHttp<'a> {
    inner: &'a dyn HttpClient,
    scope: &'a Scope,
    /// A credential being checked, sent in place of the stored one.
    candidate: Option<&'a Secret>,
}

impl HttpClient for AuthorizedHttp<'_> {
    fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
        let scope = self.scope;
        if !scope
            .origin
            .as_ref()
            .is_some_and(|origin| origin.serves(request.url))
        {
            return self.inner.send(request);
        }
        let stored;
        let secret = if let Some(candidate) = self.candidate {
            Some(candidate)
        } else {
            stored = scope.credential(request.url)?;
            stored.as_deref()
        };
        let mut scoped = request.clone();
        scoped.quota_headers = scope.headers.quota.as_slice();
        if scope.identify {
            // What identifies MSBE is the registry's to fill, never the adapter's.
            scoped.headers.retain(|header| {
                !IDENTITY
                    .iter()
                    .any(|(name, _)| header.name.eq_ignore_ascii_case(name))
            });
            scoped.headers.extend(
                IDENTITY
                    .iter()
                    .map(|(name, value)| Header::new(name, value)),
            );
        }
        if let Some(name) = &scope.headers.credential {
            // The credential header is the registry's to fill, never the adapter's.
            scoped
                .headers
                .retain(|header| !header.name.eq_ignore_ascii_case(name));
            if let Some(secret) = secret {
                scoped
                    .headers
                    .push(Header::credential(name, secret.expose()));
            }
        }
        let response = self.inner.send(&scoped)?;
        scope.keys.record(&scope.provider, &response.rate);
        Ok(response)
    }

    /// Artifacts never carry a credential, not even from the metadata origin.
    fn download(&self, request: &HttpRequest<'_>, sink: &mut dyn Write) -> Result<u64, HttpError> {
        self.inner.download(request, sink)
    }
}

impl Scope {
    /// The provider's credential, when it declares a header for one and has one.
    fn credential(&self, url: &str) -> Result<Option<Arc<Secret>>, HttpError> {
        if self.headers.credential.is_none() {
            return Ok(None);
        }
        self.keys
            .credential(&self.provider)
            .map_err(|error| HttpError::Credential {
                url: without_query(url).to_owned(),
                reason: error.to_string(),
            })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
#[path = "authorized_tests.rs"]
mod tests;
