//! Provider sign-in and terms, and what clients show about providers
//! (`docs/07-browser-and-secrets.md` §7.5). Shared by the daemon's `auth.*` and `provider.list`
//! methods. `msbe auth` reaches them only through the daemon, which alone holds secrets.

use msbe_core::config::Home;
use msbe_provider_api::{
    HttpClient,
    manifest::{Acquisition, SourceMatcher},
};
use msbe_providers::{ProviderSummary, Providers, RegistryError};
use msbe_rpc_schema::{AuthStatus, ProviderInfo};
use msbe_secrets::{Acknowledgements, Clock, Credentials, Secret, StoreError};

/// Why sign-in or terms could not be read or changed.
#[derive(Debug, thiserror::Error)]
pub enum AccountsError {
    /// No enabled provider has this id.
    #[error("no enabled provider is named {0:?}")]
    UnknownProvider(String),
    /// The provider refused the key, cannot check one, or its terms are not acknowledged.
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// The key or acknowledgement cannot be kept or read.
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Every enabled provider, as clients describe it.
pub fn provider_infos(providers: &Providers) -> Vec<ProviderInfo> {
    providers
        .summaries()
        .iter()
        .map(|summary| {
            let provider = summary.provider;
            ProviderInfo {
                id: provider.id.clone(),
                name: provider.name.clone(),
                prefix: match &provider.source {
                    SourceMatcher::Prefixed { prefix } => Some(prefix.clone()),
                    SourceMatcher::HttpsUrl => None,
                },
                search: summary.searchable,
                acquisition: match &provider.acquisition {
                    Acquisition::DirectHttps {} => "direct_https",
                    Acquisition::UserAction {} => "user_action",
                    Acquisition::BrowserAssisted { .. } => "browser_assisted",
                    Acquisition::ExternalTool {} => "external_tool",
                }
                .to_owned(),
                requires_auth: provider.policy.requires_auth,
                signed_in: summary.authenticated,
                ack_required: provider.policy.ack_required,
                acknowledged: summary.acknowledged,
            }
        })
        .collect()
}

/// Every provider that needs signing in or accepting terms, or accepts a key.
pub fn auth_statuses(providers: &Providers, credentials: &Credentials) -> Vec<AuthStatus> {
    providers
        .summaries()
        .iter()
        .filter(|summary| {
            summary.provider.policy.requires_auth
                || summary.provider.policy.ack_required
                || summary.auth.is_some()
        })
        .map(|summary| status(summary, credentials))
        .collect()
}

/// `provider`'s sign-in and terms.
///
/// # Errors
///
/// Returns [`AccountsError::UnknownProvider`] when no enabled provider has that id.
pub fn auth_status(
    providers: &Providers,
    credentials: &Credentials,
    provider: &str,
) -> Result<AuthStatus, AccountsError> {
    providers
        .summaries()
        .iter()
        .find(|summary| summary.provider.id == provider)
        .map(|summary| status(summary, credentials))
        .ok_or_else(|| AccountsError::UnknownProvider(provider.to_owned()))
}

/// Records that `provider`'s current terms, under its current program, were acknowledged.
///
/// # Errors
///
/// Returns [`AccountsError`] for an unknown provider, or an acknowledgement that cannot be kept.
pub fn acknowledge_terms(
    providers: &Providers,
    home: &Home,
    provider: &str,
    clock: &dyn Clock,
) -> Result<(), AccountsError> {
    let summaries = providers.summaries();
    let summary = summaries
        .iter()
        .find(|summary| summary.provider.id == provider)
        .ok_or_else(|| AccountsError::UnknownProvider(provider.to_owned()))?;
    Acknowledgements::load(home)?.acknowledge(
        provider,
        &summary.provider.policy.tos_url,
        providers.program_digest(provider),
        clock,
    )?;
    Ok(())
}

/// Checks `token` with `provider` and keeps it with the account the provider reported.
///
/// # Errors
///
/// Returns [`AccountsError::Registry`] when the provider refuses the key, cannot check one, or its
/// terms are not acknowledged, and [`AccountsError::Store`] when the key cannot be kept.
pub fn sign_in(
    providers: &Providers,
    credentials: &mut Credentials,
    http: &dyn HttpClient,
    provider: &str,
    token: &Secret,
) -> Result<(), AccountsError> {
    let account = providers.validate_credential(provider, http, token)?;
    credentials.store(provider, token, Some(account.name))?;
    Ok(())
}

/// Forgets the key kept for `provider`, reporting whether there was one.
///
/// # Errors
///
/// Returns [`AccountsError::Store`] when the store holding it cannot be written.
pub fn sign_out(credentials: &mut Credentials, provider: &str) -> Result<bool, AccountsError> {
    Ok(credentials.remove(provider)?)
}

fn status(summary: &ProviderSummary<'_>, credentials: &Credentials) -> AuthStatus {
    let provider = summary.provider;
    let record = credentials
        .records()
        .iter()
        .find(|record| record.provider == provider.id);
    let environment = credentials.in_environment(&provider.id);
    AuthStatus {
        provider: provider.id.clone(),
        name: provider.name.clone(),
        requires_auth: provider.policy.requires_auth,
        signed_in: environment || record.is_some(),
        source: if environment {
            Some("environment".to_owned())
        } else {
            record.map(|record| record.backend.to_string())
        },
        account: record
            .filter(|_| !environment)
            .and_then(|record| record.account.clone()),
        last_used: record
            .filter(|_| !environment)
            .and_then(|record| record.last_used),
        key_page: summary.auth.map(|auth| auth.key_page.clone()),
        terms: provider.policy.tos_url.clone(),
        ack_required: provider.policy.ack_required,
        acknowledged: summary.acknowledged,
        quota: summary
            .rate
            .as_ref()
            .map(|rate| rate.remaining.clone())
            .unwrap_or_default(),
    }
}
