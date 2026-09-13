//! Intake for browser-assisted provider links.

use std::fs;

use msbe_core::config::Home;
use msbe_provider_api::{ArtifactDescriptor, DOWNLOAD_LIMIT, acquire, model::Download};
use msbe_providers::Providers;
use msbe_rpc_schema::Response;
use msbe_secrets::Clock;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Connector;

/// The public receipt for an accepted handoff. It deliberately excludes the input URI and its
/// temporary query values.
#[derive(Debug, Serialize)]
pub(crate) struct Receipt {
    provider: String,
    game: String,
    project: String,
    release: String,
    name: String,
    size: u64,
    sha256: String,
    sha512: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Submit {
    uri: String,
}

/// Handles a handoff link from the OS, browser host, or another local client.
pub(crate) fn submit(
    params: &Value,
    home: Result<&Home, &msbe_core::instance::InstanceError>,
    connect: &Connector,
    clock: &dyn Clock,
) -> Result<Receipt, String> {
    let request: Submit =
        serde_json::from_value(params.clone()).map_err(|error| format!("{error}"))?;
    let home = home.map_err(ToString::to_string)?;
    let providers = Providers::installed(home).map_err(|error| error.to_string())?;
    let handoff = providers
        .handoff(&request.uri)
        .map_err(|error| error.to_string())?;
    let ticket = handoff
        .parse(&request.uri, clock.now())
        .map_err(|error| error.to_string())?;
    let client = connect().map_err(|error| error.to_string())?;
    let file = handoff
        .redeem(client.as_ref(), &ticket)
        .map_err(|error| error.to_string())?;
    let Download::Direct { url } = &file.download else {
        return Err("handoff redemption did not return a direct download".to_owned());
    };
    let directory = home.root().join("handoffs");
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let acquired = acquire(
        client.as_ref(),
        &ArtifactDescriptor {
            url: url.clone(),
            file_name: file.name.clone(),
            limit: file.limit.unwrap_or(DOWNLOAD_LIMIT),
            size: file.size,
            md5: file.md5.clone(),
            sha1: file.sha1.clone(),
            sha256: file.sha256.clone(),
            sha512: file.sha512.clone(),
        },
        &directory,
    )
    .map_err(|error| error.to_string())?;
    Ok(Receipt {
        provider: ticket.provider,
        game: ticket.game,
        project: ticket.project,
        release: ticket.release,
        name: file.name,
        size: acquired.size,
        sha256: acquired.sha256,
        sha512: acquired.sha512,
    })
}

/// Maps submission failures to the standard invalid-parameter response without echoing a URI.
pub(crate) fn respond(id: Value, result: Result<Receipt, String>) -> Response {
    match result {
        Ok(receipt) => match serde_json::to_value(receipt) {
            Ok(value) => Response::success(id, value),
            Err(error) => Response::error(id, -32603, error.to_string()),
        },
        Err(error) => Response::error(id, -32602, error),
    }
}
