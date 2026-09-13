//! Intake for browser-assisted provider links.

use msbe_rpc_schema::{HandoffReceipt, Response};
use msbe_secrets::Clock;
use serde::Deserialize;
use serde_json::Value;

use crate::Lanes;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Submit {
    uri: String,
}

/// Reads a link from the OS, the browser host, or another local client, and hands it to the
/// download queue's link lane.
pub(crate) fn submit(
    params: &Value,
    lanes: &Lanes,
    clock: &dyn Clock,
) -> Result<HandoffReceipt, String> {
    let request: Submit =
        serde_json::from_value(params.clone()).map_err(|error| error.to_string())?;
    lanes.submit_link(request.uri, clock)
}

/// Maps submission failures to the standard invalid-parameter response without echoing a URI.
pub(crate) fn respond(id: Value, result: Result<HandoffReceipt, String>) -> Response {
    match result {
        Ok(receipt) => match serde_json::to_value(receipt) {
            Ok(value) => Response::success(id, value),
            Err(error) => Response::error(id, -32603, error.to_string()),
        },
        Err(error) => Response::error(id, -32602, error),
    }
}
