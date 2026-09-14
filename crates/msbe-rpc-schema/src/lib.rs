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

/// Submits a browser-assisted provider link: `{ uri }` returns a [`HandoffReceipt`]. The link is
/// redeemed and downloaded at once, because its key expires; the profile change waits its turn.
pub const HANDOFF_SUBMIT_METHOD: &str = "handoff.submit";

/// Reports the MSBE browser and the downloads waiting on a page: returns a [`BrowserStatus`].
pub const BROWSER_STATUS_METHOD: &str = "browser.status";

/// Sends the MSBE browser to the page a download waits on, starting it when needed:
/// [`BrowserOpen`] returns the [`BrowserStatus`]. With no `id` it goes to the next download waiting
/// on a page after the current one, except that `auto_advance` alone, while the browser shows a
/// waiting page, changes that setting and stays on the page.
pub const BROWSER_OPEN_METHOD: &str = "browser.open";

/// Closes the MSBE browser: returns the [`BrowserStatus`].
pub const BROWSER_CLOSE_METHOD: &str = "browser.close";

/// Lists every enabled provider that runs an external tool, and the program registered for each:
/// returns a [`ToolStatus`] for each.
pub const TOOL_LIST_METHOD: &str = "tool.list";

/// Registers the program the user installed for a tool provider, pinning its SHA-256, and records
/// that the provider's terms were accepted: [`ToolRegister`] returns its [`ToolStatus`].
pub const TOOL_REGISTER_METHOD: &str = "tool.register";

/// Forgets the program registered for a tool provider: [`ToolProvider`] returns its [`ToolStatus`].
pub const TOOL_FORGET_METHOD: &str = "tool.forget";

/// Reports which application opens links: [`HandlerStatusRequest`] returns a [`HandlerStatus`]
/// for the scheme named, or one for every scheme an enabled provider hands links over in.
pub const HANDLER_STATUS_METHOD: &str = "handler.status";

/// Makes MSBE open a scheme's links for the current user: [`HandlerRegister`] returns its
/// [`HandlerStatus`]. While another application opens them it is refused with
/// [`codes::HANDLER_OWNED`], unless `replace` is set; unregistering gives them back.
pub const HANDLER_REGISTER_METHOD: &str = "handler.register";

/// Stops MSBE opening a scheme's links and gives them back to the application it replaced:
/// [`HandlerScheme`] returns its [`HandlerStatus`].
pub const HANDLER_UNREGISTER_METHOD: &str = "handler.unregister";

/// Queues a source to download and add to a profile: [`DownloadEnqueue`] returns the
/// [`DownloadItem`], or the unfinished item already queued for the same source and profile.
pub const DOWNLOAD_ENQUEUE_METHOD: &str = "download.enqueue";

/// Reads the download queue: `{ after? }` returns a [`DownloadList`] holding the items changed
/// since the revision `after`. A cursor poll, like [`JOB_EVENTS_METHOD`].
pub const DOWNLOAD_LIST_METHOD: &str = "download.list";

/// Holds one item, or with no `id` the whole queue, before its next network step: [`DownloadPause`].
pub const DOWNLOAD_PAUSE_METHOD: &str = "download.pause";

/// Releases one paused item, or with no `id` the whole queue: [`DownloadPause`].
pub const DOWNLOAD_RESUME_METHOD: &str = "download.resume";

/// Cancels an unfinished item that is not being added: [`DownloadItemId`].
pub const DOWNLOAD_CANCEL_METHOD: &str = "download.cancel";

/// Queues a failed or cancelled item again, keeping the files it already downloaded:
/// [`DownloadItemId`].
pub const DOWNLOAD_RETRY_METHOD: &str = "download.retry";

/// Moves an item to a position in the queue order: [`DownloadMove`] returns the [`DownloadList`].
pub const DOWNLOAD_MOVE_METHOD: &str = "download.move";

/// Chooses the profile for an item a link created on its own: [`DownloadConfirm`].
pub const DOWNLOAD_CONFIRM_METHOD: &str = "download.confirm";

/// Removes completed, failed and cancelled items and their downloaded files; returns the
/// [`DownloadList`].
pub const DOWNLOAD_CLEAR_METHOD: &str = "download.clear";

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
    /// Another application opens the scheme's links. `data.scheme` and `data.owner` name them; ask
    /// the user, and register again with `replace` if they agree.
    pub const HANDLER_OWNED: i32 = -32040;
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

/// Where a download queue item is.
///
/// ```text
/// queued → resolving → downloading | awaiting_user → downloaded → adding
///        → completed | failed | cancelled
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DownloadState {
    /// Waiting for the network lane.
    Queued,
    /// Held by the user until resumed.
    Paused,
    /// Its source is being resolved to the files it needs.
    Resolving,
    /// Files are being downloaded into quarantine.
    Downloading,
    /// Every file left needs the user to start its download on a provider page.
    AwaitingUser {
        /// The page of the first file waiting.
        page: String,
        /// The URI scheme of the link that page hands over. A page without one hands over the
        /// file itself, which the MSBE browser captures.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scheme: Option<String>,
    },
    /// Every file is in quarantine; the item waits for the instance lane, or for its profile to
    /// be confirmed.
    Downloaded,
    /// Its files are being added to the profile.
    Adding,
    /// Its files were added to the profile.
    Completed,
    /// It stopped with an error, and can be retried.
    Failed {
        /// What went wrong.
        message: String,
    },
    /// The user cancelled it, and can retry it.
    Cancelled,
}

impl DownloadState {
    /// Whether nothing more happens to the item unless it is retried.
    pub const fn is_finished(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed { .. } | Self::Cancelled
        )
    }

    /// The state's name, as it is serialized.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Paused => "paused",
            Self::Resolving => "resolving",
            Self::Downloading => "downloading",
            Self::AwaitingUser { .. } => "awaiting_user",
            Self::Downloaded => "downloaded",
            Self::Adding => "adding",
            Self::Completed => "completed",
            Self::Failed { .. } => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Where one file of a download queue item is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DownloadFileState {
    /// Not downloaded yet.
    Pending,
    /// The user starts its download on a provider page, which hands MSBE a link.
    AwaitingUser {
        /// The page to start the download on.
        page: String,
        /// The URI scheme of the link the page hands over. A page without one hands over the file
        /// itself, which the MSBE browser captures.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scheme: Option<String>,
    },
    /// Being downloaded.
    Downloading,
    /// In quarantine and verified.
    Downloaded,
}

/// The profile a download queue item is added to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadTarget {
    /// The instance.
    pub instance: String,
    /// The profile.
    pub profile: String,
}

/// One file of a download queue item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadFile {
    /// The provider.
    pub provider: String,
    /// The provider's project id.
    pub project: String,
    /// The provider's release id.
    pub release: String,
    /// The file name. Empty for a link that arrived on its own until it is redeemed.
    pub name: String,
    /// Its size in bytes, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Where it is.
    pub state: DownloadFileState,
}

/// One entry in the download queue: a requested source and the group of files it resolved to,
/// which are added to the profile together once every one has downloaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadItem {
    /// The daemon's identifier for it.
    pub id: u64,
    /// The queue revision that last changed it.
    pub revision: u64,
    /// A display title the client that queued it supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The source it was queued for, absent for an item a link created on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The profile it is added to, absent until confirmed for an item a link created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<DownloadTarget>,
    /// Whether required dependencies are resolved into its group.
    #[serde(default)]
    pub with_deps: bool,
    /// How many times it has been resolved or redeemed.
    pub attempts: u32,
    /// Where it is.
    pub state: DownloadState,
    /// The files it resolved to.
    #[serde(default)]
    pub files: Vec<DownloadFile>,
    /// The mods it added.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    /// Mods already in the profile, which it left as they were.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    /// Requirements resolution could not meet, and declared incompatibilities.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// The download queue as [`DOWNLOAD_LIST_METHOD`] reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadList {
    /// The current revision, to pass as `after` next time.
    pub next: u64,
    /// Whether the network lane is paused. Links are still redeemed, because their keys expire.
    pub paused: bool,
    /// Every item's ID, in queue order. An item missing from it was cleared.
    pub order: Vec<u64>,
    /// The items changed after the requested revision.
    pub items: Vec<DownloadItem>,
}

/// Parameters of [`DOWNLOAD_ENQUEUE_METHOD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadEnqueue {
    /// The instance.
    pub instance: String,
    /// The profile.
    pub profile: String,
    /// A `<provider>:<project>[@<version>]` reference or an https URL.
    pub source: String,
    /// Whether to resolve required dependencies into the item's group.
    #[serde(default)]
    pub with_deps: bool,
    /// A display title to keep with the item.
    #[serde(default)]
    pub title: Option<String>,
}

/// Parameters of [`DOWNLOAD_LIST_METHOD`]; `null` reads the whole queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadListRequest {
    /// The revision the client last saw.
    #[serde(default)]
    pub after: u64,
}

/// Parameters naming one download queue item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadItemId {
    /// The item.
    pub id: u64,
}

/// Parameters of [`DOWNLOAD_PAUSE_METHOD`] and [`DOWNLOAD_RESUME_METHOD`]; `null` means the
/// whole queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadPause {
    /// The item, or none for the whole queue.
    #[serde(default)]
    pub id: Option<u64>,
}

/// Parameters of [`DOWNLOAD_MOVE_METHOD`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadMove {
    /// The item.
    pub id: u64,
    /// Its new zero-based position.
    pub position: usize,
}

/// Parameters of [`DOWNLOAD_CONFIRM_METHOD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadConfirm {
    /// The item a link created.
    pub id: u64,
    /// The instance to add it to.
    pub instance: String,
    /// The profile to add it to.
    pub profile: String,
}

/// What [`HANDOFF_SUBMIT_METHOD`] accepted. It never repeats the link, whose query carries a key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffReceipt {
    /// The queue item the link fills.
    pub id: u64,
    /// The provider.
    pub provider: String,
    /// The plan game the link names.
    pub game: String,
    /// The provider's project id.
    pub project: String,
    /// The provider's release id.
    pub release: String,
    /// Whether the link fills a file an item was waiting on. A link that does not creates an item
    /// whose profile must be confirmed with [`DOWNLOAD_CONFIRM_METHOD`].
    pub matched: bool,
}

/// The MSBE browser, as [`BROWSER_STATUS_METHOD`] reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserStatus {
    /// Whether the browser window is open.
    pub running: bool,
    /// The provider whose pages it shows, while it is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The download whose page it was sent to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<u64>,
    /// The page it was sent to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    /// Where that page is among the files waiting on a page, counting from one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u64>,
    /// How many files wait on a page.
    pub waiting: u64,
    /// The URL the browser shows, which the user may have followed away from the page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The title of the page the browser shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Whether the browser goes to the next page once a download or link arrives.
    pub auto_advance: bool,
    /// Why the last capture was refused, or why the browser stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Parameters of [`BROWSER_OPEN_METHOD`]; `null` goes to the next page.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserOpen {
    /// The download whose page to show.
    #[serde(default)]
    pub id: Option<u64>,
    /// Whether to go to the next page once a download or link arrives. Unchanged when absent.
    #[serde(default)]
    pub auto_advance: Option<bool>,
}

/// Whether a tool provider's registered program can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolState {
    /// No program is registered.
    Unregistered,
    /// The registered program is there and still has the SHA-256 it was registered with.
    Registered,
    /// The registered program's SHA-256 changed since it was registered, so it does not run until
    /// it is registered again.
    Changed,
    /// Nothing is at the registered program's path.
    Missing,
}

/// A tool provider, as [`TOOL_LIST_METHOD`] reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStatus {
    /// The provider.
    pub provider: String,
    /// The provider's display name.
    pub name: String,
    /// The terms registering the program accepts.
    pub terms: String,
    /// The registered program.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    /// The SHA-256 the program was registered with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Whether the program can run.
    pub state: ToolState,
}

/// Parameters of [`TOOL_REGISTER_METHOD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRegister {
    /// The tool provider.
    pub provider: String,
    /// The absolute path of the program the user installed.
    pub program: String,
    /// Whether the user accepts the provider's terms, which registering requires.
    #[serde(default)]
    pub accept_terms: bool,
}

/// Parameters of [`TOOL_FORGET_METHOD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolProvider {
    /// The tool provider.
    pub provider: String,
}

/// Which application opens a scheme's links.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HandlerOwner {
    /// None does.
    Nobody,
    /// MSBE does.
    Msbe,
    /// Another application does.
    Other {
        /// The application, as the platform names it.
        name: String,
    },
}

/// A scheme's link handler registration, as [`HANDLER_STATUS_METHOD`] reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandlerStatus {
    /// The scheme, lowercase and without `://`.
    pub scheme: String,
    /// The enabled provider whose links use the scheme, if one does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Which application opens its links.
    pub owner: HandlerOwner,
    /// Whether MSBE's registration opens links with this installation's `msbe`. False when MSBE is
    /// not registered, or its registration names a program that has since moved.
    pub current: bool,
    /// The application MSBE replaced, which unregistering gives the scheme back to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
}

/// Parameters of [`HANDLER_STATUS_METHOD`]; `null` reports every scheme an enabled provider claims.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandlerStatusRequest {
    /// The scheme.
    #[serde(default)]
    pub scheme: Option<String>,
}

/// Parameters of [`HANDLER_REGISTER_METHOD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandlerRegister {
    /// The scheme. An enabled provider must hand links over in it.
    pub scheme: String,
    /// Whether to take the scheme over from the application that opens its links.
    #[serde(default)]
    pub replace: bool,
}

/// Parameters of [`HANDLER_UNREGISTER_METHOD`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandlerScheme {
    /// The scheme.
    pub scheme: String,
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
    fn handler_statuses_tag_their_owner_and_omit_what_is_not_known() -> Result<(), serde_json::Error>
    {
        let status = super::HandlerStatus {
            scheme: "handoff".to_owned(),
            provider: None,
            owner: super::HandlerOwner::Other {
                name: "Other".to_owned(),
            },
            current: false,
            previous: None,
        };
        let encoded = serde_json::to_value(&status)?;
        assert_eq!(
            encoded,
            json!({ "scheme": "handoff", "owner": { "kind": "other", "name": "Other" }, "current": false })
        );
        assert_eq!(
            serde_json::from_value::<super::HandlerStatus>(encoded)?,
            status
        );
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
