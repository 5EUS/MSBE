//! The RPC contract.
//!
//! Single source of truth for the daemon protocol. Generates Rust server traits, C#
//! DTOs with a source-generated `JsonSerializerContext`, and a JSON Schema for
//! third-party clients and contract tests.
//!
//! See `docs/03-architecture.md` §3.4.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(unix)]
use std::path::PathBuf;

/// The JSON-RPC version understood by this contract.
pub const JSON_RPC_VERSION: &str = "2.0";

/// The RPC contract with runtime game-support discovery.
pub const CONTRACT_VERSION: u32 = 3;

/// The method that reports daemon identity and contract compatibility.
pub const INFO_METHOD: &str = "daemon.info";

/// The method that executes an MSBE command under the daemon's serialized ownership.
pub const COMMAND_METHOD: &str = "command.run";

/// The method that lists games supported by the daemon's loaded plans.
pub const GAME_LIST_METHOD: &str = "game.list";

/// The developer method that loads or reloads one plan from the runtime registry directory.
pub const PLAN_LOAD_METHOD: &str = "plan.load";

/// The developer method that unloads one plan from the runtime registry.
pub const PLAN_UNLOAD_METHOD: &str = "plan.unload";

/// Returns the local daemon endpoint for the current user.
#[cfg(unix)]
pub fn default_socket() -> PathBuf {
    #[expect(
        clippy::disallowed_methods,
        reason = "the transport endpoint is configured here and nowhere else"
    )]
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    runtime_dir
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("msbe.sock")
}

/// Reports the daemon's version and its negotiated RPC contract version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DaemonInfo {
    /// The daemon package version.
    pub version: String,
    /// The highest RPC contract version the daemon understands.
    pub rpc_version: u32,
}

/// A JSON-RPC request framed as one JSON object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The JSON-RPC protocol version.
    pub jsonrpc: String,
    /// The client-selected identifier echoed by the response.
    #[serde(default)]
    pub id: Value,
    /// The RPC method to invoke.
    pub method: String,
    /// Method-specific parameters.
    #[serde(default)]
    pub params: Value,
}

impl Request {
    /// Creates a versioned request with `id`, `method` and `params`.
    pub fn new(id: Value, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSON_RPC_VERSION.to_owned(),
            id,
            method: method.into(),
            params,
        }
    }

    /// Returns whether this request uses the supported JSON-RPC version.
    pub fn is_versioned(&self) -> bool {
        self.jsonrpc == JSON_RPC_VERSION
    }
}

/// A JSON-RPC response.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Response {
    /// A successful method result.
    Success {
        /// The JSON-RPC protocol version.
        jsonrpc: &'static str,
        /// The identifier copied from the request.
        id: Value,
        /// The method result.
        result: Value,
    },
    /// A protocol or method error.
    Error {
        /// The JSON-RPC protocol version.
        jsonrpc: &'static str,
        /// The identifier copied from the request, or null for malformed input.
        id: Value,
        /// Details suitable for a client to act on.
        error: Error,
    },
}

impl Response {
    /// Creates a successful response.
    pub fn success(id: Value, result: Value) -> Self {
        Self::Success {
            jsonrpc: JSON_RPC_VERSION,
            id,
            result,
        }
    }

    /// Creates an error response.
    pub fn error(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self::Error {
            jsonrpc: JSON_RPC_VERSION,
            id,
            error: Error {
                code,
                message: message.into(),
            },
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Error {
    /// The stable JSON-RPC error code.
    pub code: i32,
    /// A human-readable error message.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{CONTRACT_VERSION, DaemonInfo, Request, Response};

    #[test]
    fn info_round_trips_with_the_csharp_naming_policy() -> Result<(), serde_json::Error> {
        let encoded = serde_json::to_string(&DaemonInfo {
            version: "0.0.0".to_owned(),
            rpc_version: CONTRACT_VERSION,
        })?;
        assert_eq!(encoded, r#"{"version":"0.0.0","rpc_version":3}"#);
        Ok(())
    }

    #[test]
    fn response_echoes_the_request_identifier() -> Result<(), serde_json::Error> {
        let request = Request::new(json!(7), "daemon.info", Value::Null);
        let response = Response::success(request.id, json!({"ready": true}));
        assert_eq!(
            serde_json::to_value(response)?,
            json!({
                "jsonrpc": "2.0",
                "id": 7,
                "result": {"ready": true}
            })
        );
        Ok(())
    }

    use serde_json::Value;
}
