//! Sign-in, terms and provider descriptors (`docs/07-browser-and-secrets.md` §7.5). A pasted key
//! becomes a [`Secret`] as soon as it is read, is checked with its provider, and is kept by the
//! credential store; no answer ever carries it.

use std::sync::Arc;

use msbe_core::config::Home;
use msbe_providers::Providers;
use msbe_rpc_schema::{
    AUTH_ACKNOWLEDGE_METHOD, AUTH_LOGIN_METHOD, AUTH_LOGOUT_METHOD, AUTH_STATUS_METHOD, AuthLogin,
    AuthProvider, PROVIDER_LIST_METHOD,
};
use msbe_secrets::{Credentials, Secret, StoreError, SystemClock};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::Connector;

/// Opens a data directory's credentials.
pub type CredentialsOpener = Arc<dyn Fn(&Home) -> Result<Credentials, StoreError> + Send + Sync>;

/// Why an accounts request was refused.
pub(crate) enum Refusal {
    /// The request or its provider is not one that can be served.
    Params(String),
    /// State could not be read or written.
    Failed(String),
    /// No such method.
    Method,
}

fn typed<T: DeserializeOwned>(params: &Value) -> Result<T, Refusal> {
    serde_json::from_value(params.clone()).map_err(|error| Refusal::Params(error.to_string()))
}

fn none(params: &Value) -> Result<(), Refusal> {
    if params.is_null() {
        Ok(())
    } else {
        Err(Refusal::Params(
            "this method takes no parameters".to_owned(),
        ))
    }
}

fn refused(error: &msbe_cli::AccountsError) -> Refusal {
    match error {
        msbe_cli::AccountsError::Store(_) => Refusal::Failed(error.to_string()),
        msbe_cli::AccountsError::UnknownProvider(_) | msbe_cli::AccountsError::Registry(_) => {
            Refusal::Params(error.to_string())
        }
    }
}

/// Serves `provider.list` and the `auth.*` methods.
pub(crate) fn handle(
    home: &Home,
    method: &str,
    params: &Value,
    open_credentials: &CredentialsOpener,
    connect: &Connector,
) -> Result<Value, Refusal> {
    let providers =
        || Providers::installed(home).map_err(|error| Refusal::Failed(error.to_string()));
    let credentials = || open_credentials(home).map_err(|error| Refusal::Failed(error.to_string()));
    let status = |provider: &str| {
        msbe_cli::auth_status(&providers()?, &credentials()?, provider)
            .map(|status| json!(status))
            .map_err(|error| refused(&error))
    };
    match method {
        PROVIDER_LIST_METHOD => {
            none(params)?;
            Ok(json!(msbe_cli::provider_infos(&providers()?)))
        }
        AUTH_STATUS_METHOD => {
            none(params)?;
            Ok(json!(msbe_cli::auth_statuses(
                &providers()?,
                &credentials()?
            )))
        }
        AUTH_ACKNOWLEDGE_METHOD => {
            let request: AuthProvider = typed(params)?;
            msbe_cli::acknowledge_terms(&providers()?, home, &request.provider, &SystemClock)
                .map_err(|error| refused(&error))?;
            status(&request.provider)
        }
        AUTH_LOGIN_METHOD => {
            let AuthLogin { provider, token } = typed(params)?;
            let token = Secret::new(token)
                .map_err(|error| Refusal::Params(format!("the key cannot be kept: {error}")))?;
            let http = connect().map_err(|error| Refusal::Failed(error.to_string()))?;
            let mut kept = credentials()?;
            msbe_cli::sign_in(&providers()?, &mut kept, http.as_ref(), &provider, &token)
                .map_err(|error| refused(&error))?;
            status(&provider)
        }
        AUTH_LOGOUT_METHOD => {
            let request: AuthProvider = typed(params)?;
            msbe_cli::sign_out(&mut credentials()?, &request.provider)
                .map_err(|error| refused(&error))?;
            status(&request.provider)
        }
        _ => Err(Refusal::Method),
    }
}
