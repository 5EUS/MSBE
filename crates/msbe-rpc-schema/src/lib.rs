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

/// The RPC contract with typed pack methods, daemon-held plans and jobs.
pub const CONTRACT_VERSION: u32 = 5;

/// The method that reports daemon identity and contract compatibility.
pub const INFO_METHOD: &str = "daemon.info";

/// The method that executes an MSBE command under the daemon's serialized ownership.
///
/// A compatibility bridge for surfaces without typed methods yet. Pack workflows use the typed
/// methods below.
pub const COMMAND_METHOD: &str = "command.run";

/// Submits a browser-assisted provider link: `{ uri }` returns its acquired artifact receipt.
pub const HANDOFF_SUBMIT_METHOD: &str = "handoff.submit";

/// The method that lists games supported by the daemon's loaded plans.
pub const GAME_LIST_METHOD: &str = "game.list";

/// The developer method that loads or reloads one plan from the runtime registry directory.
pub const PLAN_LOAD_METHOD: &str = "plan.load";

/// The developer method that unloads one plan from the runtime registry.
pub const PLAN_UNLOAD_METHOD: &str = "plan.unload";

/// Queues a job: `{ method, params }` returns `{ job_id }`.
pub const JOB_START_METHOD: &str = "job.start";

/// Reads a job's state and the events after a sequence: `{ job_id, after }`.
pub const JOB_EVENTS_METHOD: &str = "job.events";

/// Asks a job to stop: `{ job_id }` returns `{ cancelled }`.
pub const JOB_CANCEL_METHOD: &str = "job.cancel";

/// Lists permitted pack codec descriptors: `{ direction?, game? }`.
pub const PACK_CODEC_LIST_METHOD: &str = "pack.codec.list";

/// Returns a codec's option schema and normalized values: `{ codec, direction?, preset? }`.
pub const PACK_CODEC_OPTIONS_METHOD: &str = "pack.codec.options";

/// Lists the codecs and provider programs installed in the data directory, and whether each runs:
/// `{ kind, path, id?, version?, signer?, digest?, status, reason? }` each.
pub const EXTENSION_LIST_METHOD: &str = "extension.list";

/// Previews an import and holds it: returns `{ plan_id, plan_digest, plan }`.
pub const PACK_IMPORT_PREVIEW_METHOD: &str = "pack.import.preview";

/// Previews a pack-layer update and holds it.
pub const PACK_UPDATE_PREVIEW_METHOD: &str = "pack.update.preview";

/// Previews an export and holds it.
pub const PACK_EXPORT_PREVIEW_METHOD: &str = "pack.export.preview";

/// Previews a capture and holds it.
pub const PACK_CAPTURE_PREVIEW_METHOD: &str = "pack.capture.preview";

/// Runs a held import plan: `{ plan_id, plan_digest }`. Job only.
pub const PACK_IMPORT_EXECUTE_METHOD: &str = "pack.import.execute";

/// Runs a held update plan. Job only.
pub const PACK_UPDATE_EXECUTE_METHOD: &str = "pack.update.execute";

/// Runs a held export plan. Job only.
pub const PACK_EXPORT_EXECUTE_METHOD: &str = "pack.export.execute";

/// Runs a held capture plan. Job only.
pub const PACK_CAPTURE_EXECUTE_METHOD: &str = "pack.capture.execute";

/// Writes an instance snapshot: `{ instance, output }`. Job only.
pub const SNAPSHOT_CREATE_METHOD: &str = "snapshot.create";

/// Restores an instance snapshot: `{ input }`. Job only.
pub const SNAPSHOT_RESTORE_METHOD: &str = "snapshot.restore";

/// Methods that run only through [`JOB_START_METHOD`].
pub const JOB_METHODS: &[&str] = &[
    PACK_IMPORT_EXECUTE_METHOD,
    PACK_UPDATE_EXECUTE_METHOD,
    PACK_EXPORT_EXECUTE_METHOD,
    PACK_CAPTURE_EXECUTE_METHOD,
    SNAPSHOT_CREATE_METHOD,
    SNAPSHOT_RESTORE_METHOD,
];

/// Application error codes beyond the JSON-RPC reserved range.
pub mod codes {
    /// A job holds the instance state; retry once it finishes.
    pub const BUSY: i32 = -32020;
    /// A pack operation failed. `data.code` carries the stable pack failure code and
    /// `data.issues` every issue.
    pub const PACK: i32 = -32030;
}

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
    /// The data directory holding MSBE's instances and state, if the platform provides one.
    ///
    /// Absent from daemons that predate it, which clients must tolerate.
    #[serde(default)]
    pub data_directory: Option<String>,
}

/// Where a job is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    /// Waiting for the worker.
    Queued,
    /// Running on the worker.
    Running,
    /// Finished with a result.
    Succeeded,
    /// Finished with a failure.
    Failed,
    /// Stopped before finishing, leaving no partial profile or output.
    Cancelled,
}

/// Something a job reported.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JobEvent {
    /// Work completed so far. Consecutive progress events coalesce into the latest.
    Progress {
        /// Steps done.
        completed: u64,
        /// Steps in total.
        total: u64,
        /// What is happening.
        message: String,
    },
    /// The job finished; `result` is the operation's report.
    Done {
        /// The operation's report.
        result: Value,
    },
    /// The job failed.
    Failed {
        /// The stable failure code.
        code: String,
        /// Human-readable detail.
        message: String,
        /// Every issue the failure reported.
        issues: Value,
    },
    /// The job was cancelled.
    Cancelled,
}

/// One event and its sequence number within its job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobEventRecord {
    /// Position in the job's event log, starting at one.
    pub sequence: u64,
    /// The event.
    #[serde(flatten)]
    pub event: JobEvent,
}

/// A job's state and the events a client has not seen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobStatus {
    /// The job.
    pub job_id: u64,
    /// The job method it runs.
    pub method: String,
    /// Where it is.
    pub state: JobState,
    /// Events after the requested sequence.
    pub events: Vec<JobEventRecord>,
    /// The sequence to pass as `after` next time.
    pub next: u64,
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
        Self::error_with_data(id, code, message, None)
    }

    /// Creates an error response carrying structured `data`.
    pub fn error_with_data(
        id: Value,
        code: i32,
        message: impl Into<String>,
        data: Option<Value>,
    ) -> Self {
        Self::Error {
            jsonrpc: JSON_RPC_VERSION,
            id,
            error: Error {
                code,
                message: message.into(),
                data,
            },
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Error {
    /// The stable JSON-RPC error code.
    pub code: i32,
    /// A human-readable error message.
    pub message: String,
    /// Structured detail, such as a stable pack failure code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{
        CONTRACT_VERSION, DaemonInfo, JobEvent, JobEventRecord, JobState, JobStatus, Request,
        Response,
    };

    #[test]
    fn info_round_trips_with_the_csharp_naming_policy() -> Result<(), serde_json::Error> {
        let info = DaemonInfo {
            version: "0.0.0".to_owned(),
            rpc_version: CONTRACT_VERSION,
            data_directory: Some("/msbe".to_owned()),
        };
        let encoded = serde_json::to_string(&info)?;
        assert_eq!(
            encoded,
            r#"{"version":"0.0.0","rpc_version":5,"data_directory":"/msbe"}"#
        );
        assert_eq!(serde_json::from_str::<DaemonInfo>(&encoded)?, info);
        Ok(())
    }

    #[test]
    fn info_from_an_older_daemon_has_no_data_directory() -> Result<(), serde_json::Error> {
        let info = serde_json::from_str::<DaemonInfo>(r#"{"version":"0.0.0","rpc_version":3}"#)?;
        assert_eq!(info.data_directory, None);
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

    #[test]
    fn job_events_flatten_their_sequence_beside_the_event_kind() -> Result<(), serde_json::Error> {
        let status = JobStatus {
            job_id: 3,
            method: "pack.export.execute".to_owned(),
            state: JobState::Running,
            events: vec![JobEventRecord {
                sequence: 1,
                event: JobEvent::Progress {
                    completed: 1,
                    total: 4,
                    message: "writing".to_owned(),
                },
            }],
            next: 1,
        };
        assert_eq!(
            serde_json::to_value(&status)?,
            json!({
                "job_id": 3,
                "method": "pack.export.execute",
                "state": "running",
                "events": [{"sequence": 1, "kind": "progress", "completed": 1, "total": 4, "message": "writing"}],
                "next": 1
            })
        );
        Ok(())
    }
}
