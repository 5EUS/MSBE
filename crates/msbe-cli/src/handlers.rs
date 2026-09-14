//! Which application opens the links provider pages hand files over in. Shared by `msbe handler`
//! and the daemon's `handler.*` methods, so both report and change registrations the same way.

use std::{collections::BTreeMap, path::Path};

use msbe_os_integration::{Handlers, Owner, Scheme, Status};
use msbe_providers::Providers;
use msbe_rpc_schema::{HandlerOwner, HandlerStatus};

/// Why a link handler registration could not be reported or changed.
#[derive(Debug, thiserror::Error)]
pub enum HandlerError {
    /// No enabled provider hands links over in the scheme, so MSBE could do nothing with them.
    #[error("no enabled provider hands files over as {0}:// links")]
    UnknownScheme(String),
    /// Another application opens the links, or the registration cannot be read or changed.
    #[error(transparent)]
    Os(#[from] msbe_os_integration::Error),
}

/// Reports which application opens `scheme`'s links, or with no scheme, those of every scheme an
/// enabled provider hands links over in.
///
/// # Errors
///
/// Returns [`HandlerError`] when the scheme is invalid or a registration cannot be read.
pub fn handler_status(
    providers: &Providers,
    handlers: &Handlers,
    scheme: Option<&str>,
    program: &Path,
) -> Result<Vec<HandlerStatus>, HandlerError> {
    let claimed = providers.handoff_schemes();
    let schemes = match scheme {
        Some(scheme) => vec![Scheme::parse(scheme)?],
        None => claimed
            .keys()
            .map(|scheme| Scheme::parse(scheme))
            .collect::<Result<_, _>>()?,
    };
    schemes
        .iter()
        .map(|scheme| Ok(report(handlers.status(scheme, program)?, &claimed)))
        .collect()
}

/// Makes `program` open `scheme`'s links for the current user. Only a scheme an enabled provider
/// hands links over in is registered, and another application's is taken only with `replace`.
///
/// # Errors
///
/// Returns [`HandlerError::UnknownScheme`] when no enabled provider claims the scheme, and
/// [`HandlerError::Os`] when another application opens the links and `replace` is not set, or the
/// registration cannot be changed.
pub fn register_handler(
    providers: &Providers,
    handlers: &Handlers,
    scheme: &str,
    replace: bool,
    program: &Path,
) -> Result<HandlerStatus, HandlerError> {
    let scheme = Scheme::parse(scheme)?;
    let claimed = providers.handoff_schemes();
    if !claimed.contains_key(scheme.as_str()) {
        return Err(HandlerError::UnknownScheme(scheme.to_string()));
    }
    Ok(report(
        handlers.register(&scheme, program, replace)?,
        &claimed,
    ))
}

/// Stops MSBE opening `scheme`'s links and gives them back. Any scheme is accepted, so a scheme
/// whose provider was removed can still be given back.
///
/// # Errors
///
/// Returns [`HandlerError`] when the scheme is invalid or the registration cannot be changed.
pub fn unregister_handler(
    providers: &Providers,
    handlers: &Handlers,
    scheme: &str,
    program: &Path,
) -> Result<HandlerStatus, HandlerError> {
    let scheme = Scheme::parse(scheme)?;
    let claimed = providers.handoff_schemes();
    Ok(report(handlers.unregister(&scheme, program)?, &claimed))
}

fn report(status: Status, claimed: &BTreeMap<String, String>) -> HandlerStatus {
    HandlerStatus {
        provider: claimed.get(status.scheme.as_str()).cloned(),
        scheme: status.scheme.to_string(),
        owner: match status.owner {
            Owner::Nobody => HandlerOwner::Nobody,
            Owner::Msbe => HandlerOwner::Msbe,
            Owner::Other(name) => HandlerOwner::Other { name },
        },
        current: status.current,
        previous: status.previous,
    }
}
