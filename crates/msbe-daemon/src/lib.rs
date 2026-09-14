//! JSON-RPC request handling for the MSBE daemon.
//!
//! The listener answers one request at a time. Long operations are jobs: `job.start` queues them
//! and a worker thread runs them one at a time while the listener keeps answering `job.events`
//! and `job.cancel`. A running job holds the instance state, so requests that read or write it
//! answer busy instead of observing a half-applied operation.

use std::{
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use msbe_core::config::Home;
use msbe_os_integration::Handlers;
use msbe_provider_api::{HttpClient, HttpError};
use msbe_providers::Providers;
use msbe_rpc_schema::{
    BROWSER_CLOSE_METHOD, BROWSER_OPEN_METHOD, BROWSER_STATUS_METHOD, BrowserOpen, COMMAND_METHOD,
    CONTRACT_VERSION, DOWNLOAD_CANCEL_METHOD, DOWNLOAD_CLEAR_METHOD, DOWNLOAD_CONFIRM_METHOD,
    DOWNLOAD_ENQUEUE_METHOD, DOWNLOAD_LIST_METHOD, DOWNLOAD_MOVE_METHOD, DOWNLOAD_PAUSE_METHOD,
    DOWNLOAD_RESUME_METHOD, DOWNLOAD_RETRY_METHOD, DaemonInfo, DownloadItemId, DownloadListRequest,
    DownloadPause, EXTENSION_LIST_METHOD, GAME_LIST_METHOD, HANDLER_REGISTER_METHOD,
    HANDLER_STATUS_METHOD, HANDLER_UNREGISTER_METHOD, HANDOFF_SUBMIT_METHOD, HandlerRegister,
    HandlerScheme, HandlerStatusRequest, INFO_METHOD, JOB_CANCEL_METHOD, JOB_EVENTS_METHOD,
    JOB_METHODS, JOB_START_METHOD, PACK_CAPTURE_PREVIEW_METHOD, PACK_CODEC_LIST_METHOD,
    PACK_CODEC_OPTIONS_METHOD, PACK_EXPORT_PREVIEW_METHOD, PACK_IMPORT_PREVIEW_METHOD,
    PACK_UPDATE_PREVIEW_METHOD, PLAN_LOAD_METHOD, PLAN_UNLOAD_METHOD, Request, Response,
    TOOL_FORGET_METHOD, TOOL_LIST_METHOD, TOOL_REGISTER_METHOD, ToolProvider, ToolRegister,
};
use msbe_secrets::SystemClock;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

mod browser;
mod downloads;
mod handoff;
mod jobs;
mod pack;
mod registry;
mod tools;

pub use browser::{Browser, Launcher, Process, installed_launcher};
pub use downloads::{Downloads, Lanes};
pub use jobs::Jobs;
pub use registry::{Game, PlanRegistry, RegistryError};

/// Opens a network client for a job that acquires content.
pub type Connector = Arc<dyn Fn() -> Result<Box<dyn HttpClient>, HttpError> + Send + Sync>;

/// The daemon's state: loaded game support, previews awaiting execution, the job queue, and the
/// download queue.
pub struct Daemon {
    registry: PlanRegistry,
    home: Option<PathBuf>,
    plans: pack::Plans,
    jobs: Arc<Jobs>,
    downloads: Arc<Downloads>,
    connect: Connector,
    handlers: Handlers,
    handler_program: Option<PathBuf>,
    browser: Arc<Browser>,
}

impl fmt::Debug for Daemon {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Daemon")
            .field("registry", &self.registry)
            .field("home", &self.home)
            .field("jobs", &self.jobs)
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// A daemon serving `registry` from the discovered data directory.
    pub fn new(registry: PlanRegistry) -> Self {
        Self {
            registry,
            home: None,
            plans: pack::Plans::default(),
            jobs: Arc::new(Jobs::default()),
            downloads: Arc::new(Downloads::default()),
            handlers: Handlers::discover(),
            handler_program: None,
            browser: Arc::new(Browser::new(installed_launcher())),
            connect: Arc::new(|| {
                msbe_http::UreqClient::connect()
                    .map(|client| -> Box<dyn HttpClient> { Box::new(client) })
            }),
        }
    }

    /// Uses `home` as the data directory instead of discovering one.
    #[must_use]
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Registers link handlers in `handlers`, opening links with `program`, instead of the current
    /// user's registrations and the `msbe` beside the daemon.
    #[must_use]
    pub fn with_handlers(mut self, handlers: Handlers, program: PathBuf) -> Self {
        self.handlers = handlers;
        self.handler_program = Some(program);
        self
    }

    /// Starts the browser process through `launcher`, instead of `msbe-browser` beside the daemon.
    #[must_use]
    pub fn with_browser(mut self, launcher: Launcher) -> Self {
        self.browser = Arc::new(Browser::new(launcher));
        self
    }

    /// Opens network clients for jobs through `connect`.
    #[must_use]
    pub fn with_connector(mut self, connect: Connector) -> Self {
        self.connect = connect;
        self
    }

    /// The job queue, for the worker thread.
    pub fn jobs(&self) -> Arc<Jobs> {
        Arc::clone(&self.jobs)
    }

    /// The download queue and what its lanes run against, for the lane threads.
    pub fn lanes(&self) -> Lanes {
        Lanes {
            downloads: Arc::clone(&self.downloads),
            jobs: Arc::clone(&self.jobs),
            home: self.home.clone(),
            connect: Arc::clone(&self.connect),
        }
    }

    /// Handles one fully decoded request.
    pub fn handle(&mut self, request: &Request) -> Response {
        if !request.is_versioned() {
            return Response::error(request.id.clone(), -32600, "expected JSON-RPC version 2.0");
        }
        let id = request.id.clone();
        match request.method.as_str() {
            INFO_METHOD => self.info(id, &request.params),
            COMMAND_METHOD => self.command(id, request.params.clone()),
            HANDOFF_SUBMIT_METHOD => handoff::respond(
                id,
                handoff::submit(&request.params, &self.lanes(), &SystemClock),
            ),
            method if method.starts_with("download.") => self.download(id, method, &request.params),
            method if method.starts_with("handler.") => self.handler(id, method, &request.params),
            method if method.starts_with("browser.") => self.browser(id, method, &request.params),
            method if method.starts_with("tool.") => self.tool(id, method, &request.params),
            GAME_LIST_METHOD => game_list(id, &request.params, &self.registry),
            PLAN_LOAD_METHOD => plan_load(id, request.params.clone(), &mut self.registry),
            PLAN_UNLOAD_METHOD => plan_unload(id, request.params.clone(), &mut self.registry),
            PACK_CODEC_LIST_METHOD => pack::respond(
                id,
                self.pack_home()
                    .and_then(|home| pack::codec_list(&request.params, &home)),
            ),
            PACK_CODEC_OPTIONS_METHOD => pack::respond(
                id,
                self.pack_home()
                    .and_then(|home| pack::codec_options(&request.params, &home)),
            ),
            EXTENSION_LIST_METHOD => pack::respond(
                id,
                self.pack_home()
                    .and_then(|home| pack::extension_list(&home)),
            ),
            method @ (PACK_IMPORT_PREVIEW_METHOD
            | PACK_UPDATE_PREVIEW_METHOD
            | PACK_EXPORT_PREVIEW_METHOD
            | PACK_CAPTURE_PREVIEW_METHOD) => self.preview(id, method, &request.params),
            JOB_START_METHOD => self.job_start(id, &request.params),
            JOB_EVENTS_METHOD => pack::respond(id, pack::job_events(&self.jobs, &request.params)),
            JOB_CANCEL_METHOD => pack::respond(id, pack::job_cancel(&self.jobs, &request.params)),
            method if JOB_METHODS.contains(&method) => Response::error(
                id,
                -32600,
                format!("{method} runs only as a job; start it with {JOB_START_METHOD}"),
            ),
            _ => Response::error(id, -32601, "method not found"),
        }
    }

    fn home(&self) -> Result<Home, msbe_core::instance::InstanceError> {
        self.home
            .as_ref()
            .map_or_else(Home::discover, |home| Ok(Home::at(home)))
    }

    /// The data directory, as a pack failure when it cannot be found.
    fn pack_home(&self) -> Result<Home, pack::Failure> {
        self.home()
            .map_err(|error| pack::Failure::from(msbe_pack::PackError::from(error)))
    }

    fn info(&self, id: Value, params: &Value) -> Response {
        if !params.is_null() {
            return Response::error(id, -32602, "daemon.info does not accept parameters");
        }
        let data_directory = self
            .home()
            .ok()
            .map(|home| home.root().to_string_lossy().into_owned());
        match serde_json::to_value(DaemonInfo {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            rpc_version: CONTRACT_VERSION,
            data_directory,
        }) {
            Ok(info) => Response::success(id, info),
            Err(error) => Response::error(id, -32603, error.to_string()),
        }
    }

    fn command(&self, id: Value, params: Value) -> Response {
        let Ok(_state) = self.jobs.try_state() else {
            return pack::respond(id, Err(pack::Failure::Busy));
        };
        let mut command = match serde_json::from_value::<Command>(params) {
            Ok(command) if !command.args.is_empty() => command,
            Ok(_) => {
                return Response::error(id, -32602, "command.run requires at least one argument");
            }
            Err(error) => return Response::error(id, -32602, error.to_string()),
        };
        if let Err(message) = resolve_game(&mut command.args, &self.registry) {
            return Response::error(id, -32010, message);
        }
        let mut output = Vec::new();
        let mut diagnostics = Vec::new();
        let mut args = Vec::with_capacity(command.args.len() + 3);
        args.push("msbe".to_owned());
        if let Some(home) = &self.home {
            args.push("--home".to_owned());
            args.push(home.display().to_string());
        }
        args.extend(command.args);
        let exit_code = msbe_cli::run(args, &mut output, &mut diagnostics);
        Response::success(
            id,
            json!({
                "exit_code": exit_code,
                "stdout": String::from_utf8_lossy(&output),
                "stderr": String::from_utf8_lossy(&diagnostics),
            }),
        )
    }

    fn preview(&mut self, id: Value, method: &str, params: &Value) -> Response {
        let Ok(_state) = self.jobs.try_state() else {
            return pack::respond(id, Err(pack::Failure::Busy));
        };
        let result = self
            .pack_home()
            .and_then(|home| pack::preview(method, params, &home))
            .and_then(|plan| self.plans.hold(plan));
        pack::respond(id, result)
    }

    fn job_start(&mut self, id: Value, params: &Value) -> Response {
        let result = pack::start(&mut self.plans, params).map(|(method, work)| {
            let job = self.jobs.submit(
                &method,
                work,
                jobs::Environment {
                    home: self.home.clone(),
                    connect: Arc::clone(&self.connect),
                    downloads: Arc::clone(&self.downloads),
                },
            );
            json!({ "job_id": job })
        });
        pack::respond(id, result)
    }

    /// The tool registration methods. They change files in the data directory, not instance state, so
    /// they never wait for a job.
    fn tool(&self, id: Value, method: &str, params: &Value) -> Response {
        let loaded = self
            .home()
            .map_err(|error| error.to_string())
            .and_then(|home| {
                Providers::installed(&home)
                    .map(|providers| (home, providers))
                    .map_err(|error| error.to_string())
            });
        let (home, providers) = match loaded {
            Ok(loaded) => loaded,
            Err(message) => return Response::error(id, -32603, message),
        };
        let result = match method {
            TOOL_LIST_METHOD => optional::<Empty>(params).map(|Empty {}| {
                msbe_cli::tool_statuses(&providers, &home).map(|statuses| json!(statuses))
            }),
            TOOL_REGISTER_METHOD => typed::<ToolRegister>(params).map(|request| {
                msbe_cli::register_tool(
                    &providers,
                    &home,
                    &request.provider,
                    Path::new(&request.program),
                    request.accept_terms,
                    &SystemClock,
                )
                .map(|status| json!(status))
            }),
            TOOL_FORGET_METHOD => typed::<ToolProvider>(params).map(|request| {
                msbe_cli::forget_tool(&providers, &home, &request.provider)
                    .map(|status| json!(status))
            }),
            _ => return Response::error(id, -32601, "method not found"),
        };
        match result {
            Ok(Ok(value)) => Response::success(id, value),
            Ok(Err(
                error @ (msbe_cli::ToolsError::UnknownTool(_)
                | msbe_cli::ToolsError::TermsNotAccepted { .. }
                | msbe_cli::ToolsError::Program { .. }),
            )) => Response::error(id, -32602, error.to_string()),
            Ok(Err(error)) => Response::error(id, -32603, error.to_string()),
            Err(message) => Response::error(id, -32602, message),
        }
    }

    /// The browser methods. They never wait for a job: the browser holds no instance state.
    fn browser(&self, id: Value, method: &str, params: &Value) -> Response {
        let lanes = self.lanes();
        match method {
            BROWSER_STATUS_METHOD => answer(
                id,
                optional::<Empty>(params).and_then(|Empty {}| self.browser.status(&lanes)),
            ),
            BROWSER_OPEN_METHOD => answer(
                id,
                optional::<BrowserOpen>(params)
                    .and_then(|request| self.browser.open(&lanes, request)),
            ),
            BROWSER_CLOSE_METHOD => answer(
                id,
                optional::<Empty>(params).and_then(|Empty {}| self.browser.close(&lanes)),
            ),
            _ => Response::error(id, -32601, "method not found"),
        }
    }

    /// The link handler methods. They change the user's desktop, not instance state, so they never
    /// wait for a job.
    fn handler(&self, id: Value, method: &str, params: &Value) -> Response {
        let program = match &self.handler_program {
            Some(program) => program.clone(),
            None => match msbe_os_integration::handler_program() {
                Ok(program) => program,
                Err(error) => return Response::error(id, -32603, error.to_string()),
            },
        };
        let providers = match self
            .home()
            .map_err(|error| error.to_string())
            .and_then(|home| Providers::installed(&home).map_err(|error| error.to_string()))
        {
            Ok(providers) => providers,
            Err(message) => return Response::error(id, -32603, message),
        };
        let handlers = &self.handlers;
        let result = match method {
            HANDLER_STATUS_METHOD => optional::<HandlerStatusRequest>(params).map(|request| {
                msbe_cli::handler_status(&providers, handlers, request.scheme.as_deref(), &program)
                    .map(|statuses| json!(statuses))
            }),
            HANDLER_REGISTER_METHOD => typed::<HandlerRegister>(params).map(|request| {
                msbe_cli::register_handler(
                    &providers,
                    handlers,
                    &request.scheme,
                    request.replace,
                    &program,
                )
                .map(|status| json!(status))
            }),
            HANDLER_UNREGISTER_METHOD => typed::<HandlerScheme>(params).map(|request| {
                msbe_cli::unregister_handler(&providers, handlers, &request.scheme, &program)
                    .map(|status| json!(status))
            }),
            _ => return Response::error(id, -32601, "method not found"),
        };
        match result {
            Ok(Ok(value)) => Response::success(id, value),
            Ok(Err(error)) => handler_failure(id, &error),
            Err(message) => Response::error(id, -32602, message),
        }
    }

    /// The download queue's methods. They never wait for a job: the queue is not instance state.
    fn download(&self, id: Value, method: &str, params: &Value) -> Response {
        let lanes = self.lanes();
        match method {
            DOWNLOAD_ENQUEUE_METHOD => answer(id, typed(params).and_then(|r| lanes.enqueue(r))),
            DOWNLOAD_LIST_METHOD => answer(
                id,
                optional::<DownloadListRequest>(params).and_then(|r| lanes.list(r.after)),
            ),
            DOWNLOAD_PAUSE_METHOD => answer(
                id,
                optional::<DownloadPause>(params).and_then(|r| lanes.pause(r.id)),
            ),
            DOWNLOAD_RESUME_METHOD => answer(
                id,
                optional::<DownloadPause>(params).and_then(|r| lanes.resume(r.id)),
            ),
            DOWNLOAD_CANCEL_METHOD => answer(
                id,
                typed::<DownloadItemId>(params).and_then(|r| lanes.cancel(r.id)),
            ),
            DOWNLOAD_RETRY_METHOD => answer(
                id,
                typed::<DownloadItemId>(params).and_then(|r| lanes.retry(r.id)),
            ),
            DOWNLOAD_MOVE_METHOD => answer(id, typed(params).and_then(|r| lanes.move_item(r))),
            DOWNLOAD_CONFIRM_METHOD => answer(id, typed(params).and_then(|r| lanes.confirm(r))),
            DOWNLOAD_CLEAR_METHOD => answer(
                id,
                optional::<Empty>(params).and_then(|Empty {}| lanes.clear()),
            ),
            _ => Response::error(id, -32601, "method not found"),
        }
    }
}

/// A method that takes no parameters.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

fn typed<T: DeserializeOwned>(params: &Value) -> Result<T, String> {
    serde_json::from_value(params.clone()).map_err(|error| error.to_string())
}

fn optional<T: DeserializeOwned + Default>(params: &Value) -> Result<T, String> {
    if params.is_null() {
        Ok(T::default())
    } else {
        typed(params)
    }
}

/// A download queue result as a response; a refusal is an invalid-parameter error.
fn answer<T: Serialize>(id: Value, result: Result<T, String>) -> Response {
    match result.map(|result| serde_json::to_value(result)) {
        Ok(Ok(result)) => Response::success(id, result),
        Ok(Err(error)) => Response::error(id, -32603, error.to_string()),
        Err(error) => Response::error(id, -32602, error),
    }
}

/// Maps a link handler failure to a response. Another application owning the scheme carries its
/// name, so a client can ask the user before registering again with `replace`.
fn handler_failure(id: Value, error: &msbe_cli::HandlerError) -> Response {
    use msbe_cli::HandlerError;
    use msbe_os_integration::Error;

    match error {
        HandlerError::Os(Error::Owned { scheme, owner }) => Response::error_with_data(
            id,
            msbe_rpc_schema::codes::HANDLER_OWNED,
            error.to_string(),
            Some(json!({ "scheme": scheme.as_str(), "owner": owner })),
        ),
        HandlerError::UnknownScheme(_) | HandlerError::Os(Error::InvalidScheme(_)) => {
            Response::error(id, -32602, error.to_string())
        }
        HandlerError::Os(_) => Response::error(id, -32603, error.to_string()),
    }
}

fn game_list(id: Value, params: &Value, registry: &PlanRegistry) -> Response {
    if !params.is_null() {
        return Response::error(id, -32602, "game.list does not accept parameters");
    }
    match serde_json::to_value(registry.games()) {
        Ok(games) => Response::success(id, games),
        Err(error) => Response::error(id, -32603, error.to_string()),
    }
}

fn plan_load(id: Value, params: Value, registry: &mut PlanRegistry) -> Response {
    let plan = match serde_json::from_value::<PlanId>(params) {
        Ok(plan) => plan,
        Err(error) => return Response::error(id, -32602, error.to_string()),
    };
    match registry.load(&plan.id) {
        Ok(game) => match serde_json::to_value(game) {
            Ok(game) => Response::success(id, game),
            Err(error) => Response::error(id, -32603, error.to_string()),
        },
        Err(error) => Response::error(id, -32011, error.to_string()),
    }
}

fn plan_unload(id: Value, params: Value, registry: &mut PlanRegistry) -> Response {
    let plan = match serde_json::from_value::<PlanId>(params) {
        Ok(plan) => plan,
        Err(error) => return Response::error(id, -32602, error.to_string()),
    };
    Response::success(id, json!({"unloaded": registry.unload(&plan.id)}))
}

fn resolve_game(args: &mut [String], registry: &PlanRegistry) -> Result<(), String> {
    let is_instance_add = args
        .windows(2)
        .any(|pair| matches!(pair, [command, action] if command == "instance" && action == "add"));
    if !is_instance_add {
        return Ok(());
    }
    let Some(index) = args.iter().position(|argument| argument == "--game") else {
        return Ok(());
    };
    let game = args
        .get(index + 1)
        .ok_or_else(|| "--game requires a game identifier".to_owned())?;
    let manifest = registry
        .manifest(game)
        .ok_or_else(|| format!("game {game} is not supported by this daemon"))?;
    let replacement = manifest.to_string_lossy();
    let pair = args
        .get_mut(index..=index + 1)
        .ok_or_else(|| "--game requires a game identifier".to_owned())?;
    let [flag, value] = pair else {
        return Err("--game requires a game identifier".to_owned());
    };
    flag.clear();
    flag.push_str("--plan");
    value.clear();
    value.push_str(&replacement);
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    args: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanId {
    id: String,
}

/// Serves newline-delimited JSON-RPC over a Unix domain socket, running jobs on a worker thread.
///
/// # Errors
///
/// Returns an error if the socket cannot be created, secured, or used to serve a client.
#[cfg(unix)]
pub fn serve(socket: &Path, plans: &Path, home: Option<PathBuf>) -> io::Result<()> {
    use std::{
        fs,
        os::{
            unix::fs::PermissionsExt,
            unix::net::{UnixListener, UnixStream},
        },
        thread,
    };

    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }
    let registry = PlanRegistry::discover(plans.to_owned()).map_err(io::Error::other)?;
    let mut daemon = Daemon::new(registry);
    if let Some(home) = home {
        daemon = daemon.with_home(home);
    }
    let listener = match UnixListener::bind(socket) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(socket).is_ok() {
                return Err(error);
            }
            #[expect(
                clippy::disallowed_methods,
                reason = "an unconnectable socket path is stale after a daemon crash"
            )]
            fs::remove_file(socket)?;
            UnixListener::bind(socket)?
        }
        Err(error) => return Err(error),
    };
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    let worker = daemon.jobs();
    thread::Builder::new()
        .name("msbe-jobs".to_owned())
        .spawn(move || {
            loop {
                worker.wait_and_run();
            }
        })?;
    let lanes = daemon.lanes();
    lanes.recover();
    let network = lanes.clone();
    thread::Builder::new()
        .name("msbe-downloads".to_owned())
        .spawn(move || {
            loop {
                network.wait_and_run();
            }
        })?;
    thread::Builder::new()
        .name("msbe-links".to_owned())
        .spawn(move || {
            loop {
                lanes.wait_and_run_link();
            }
        })?;
    for stream in listener.incoming() {
        serve_stream(stream?, &mut daemon)?;
    }
    Ok(())
}

#[cfg(unix)]
fn serve_stream(stream: std::os::unix::net::UnixStream, daemon: &mut Daemon) -> io::Result<()> {
    use std::io::{BufRead, BufReader, Write};

    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        let response = match serde_json::from_str(&line) {
            Ok(request) => daemon.handle(&request),
            Err(error) => Response::error(Value::Null, -32700, error.to_string()),
        };
        serde_json::to_writer(&mut writer, &redacted(&response)?).map_err(io::Error::other)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    Ok(())
}

/// `response` as JSON, with every secret the daemon has held redacted from its strings. This is
/// the filter in front of everything the daemon writes (`docs/07-browser-and-secrets.md` §7.5),
/// so no handler has to remember to redact.
#[cfg(any(unix, test))]
fn redacted(response: &Response) -> io::Result<Value> {
    let mut value = serde_json::to_value(response).map_err(io::Error::other)?;
    redact_strings(&mut value);
    Ok(value)
}

#[cfg(any(unix, test))]
fn redact_strings(value: &mut Value) {
    use std::borrow::Cow;

    match value {
        Value::String(text) => {
            let clean = match msbe_secrets::redact::redact(text) {
                Cow::Owned(clean) => Some(clean),
                Cow::Borrowed(_) => None,
            };
            if let Some(clean) = clean {
                *text = clean;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_strings),
        Value::Object(fields) => fields.values_mut().for_each(redact_strings),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        io::Write,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    };

    use msbe_provider_api::{
        ExtensionCapability, ExtensionEnvelope, ExtensionProvide, HostApiRange, HttpClient,
        HttpError, HttpRequest, HttpResponse, ProviderProgram, ProviderProgramEnvelope, SigningKey,
        hex,
    };
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::{
        COMMAND_METHOD, Connector, Daemon, EXTENSION_LIST_METHOD, GAME_LIST_METHOD,
        HANDOFF_SUBMIT_METHOD, INFO_METHOD, Jobs, PLAN_UNLOAD_METHOD, PlanRegistry,
    };
    use msbe_rpc_schema::{
        CONTRACT_VERSION, DOWNLOAD_CANCEL_METHOD, DOWNLOAD_CLEAR_METHOD, DOWNLOAD_CONFIRM_METHOD,
        DOWNLOAD_ENQUEUE_METHOD, DOWNLOAD_LIST_METHOD, DOWNLOAD_MOVE_METHOD, DOWNLOAD_PAUSE_METHOD,
        DOWNLOAD_RESUME_METHOD, DOWNLOAD_RETRY_METHOD, JOB_CANCEL_METHOD, JOB_EVENTS_METHOD,
        JOB_START_METHOD, PACK_CODEC_LIST_METHOD, PACK_EXPORT_EXECUTE_METHOD,
        PACK_EXPORT_PREVIEW_METHOD, PLAN_LOAD_METHOD, Request, Response, SNAPSHOT_CREATE_METHOD,
        codes,
    };

    const PLAN: &str = r#"
schema = 1
id = "example"
name = "Example Game"
version = "1.0.0"

[[loaders]]
id = "native"
bootstrap = "none"
"#;

    fn registry() -> (TempDir, PlanRegistry) {
        let directory = TempDir::new().unwrap();
        let game = directory.path().join("example");
        fs::create_dir(&game).unwrap();
        fs::write(game.join("plan.toml"), PLAN).unwrap();
        let registry = PlanRegistry::discover(directory.path().to_owned()).unwrap();
        (directory, registry)
    }

    fn call(daemon: &mut Daemon, method: &str, params: Value) -> Response {
        daemon.handle(&Request::new(json!(1), method, params))
    }

    fn success(response: Response) -> Value {
        match response {
            Response::Success { result, .. } => result,
            Response::Error { error, .. } => panic!("expected success, got {error:?}"),
        }
    }

    fn text<'a>(value: &'a Value, key: &str) -> &'a str {
        value.get(key).and_then(Value::as_str).unwrap()
    }

    #[test]
    fn extension_list_reports_each_installed_extension_and_why_one_is_refused() {
        let (_plans, registry) = registry();
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        let mut daemon = Daemon::new(registry).with_home(home.clone());
        assert_eq!(
            success(call(&mut daemon, EXTENSION_LIST_METHOD, Value::Null)),
            json!([])
        );

        fs::create_dir_all(home.join("extensions/providers")).unwrap();
        fs::write(
            home.join("extensions/providers/broken.toml"),
            "not an envelope",
        )
        .unwrap();
        let listed = success(call(&mut daemon, EXTENSION_LIST_METHOD, Value::Null));
        let [broken] = listed.as_array().unwrap().as_slice() else {
            panic!("{listed}");
        };
        assert_eq!(text(broken, "kind"), "program");
        assert_eq!(text(broken, "status"), "refused");
        assert!(text(broken, "path").ends_with("broken.toml"), "{broken}");
        assert!(!text(broken, "reason").is_empty());
    }

    #[test]
    fn responses_are_written_with_every_held_secret_redacted() {
        let secret = msbe_secrets::Secret::new("daemon-held-token-7".to_owned()).unwrap();
        let response = Response::error_with_data(
            json!(1),
            -32603,
            format!("request with {} failed", secret.expose()),
            Some(json!({ "issues": [{ "detail": ["nested", secret.expose()] }] })),
        );
        let written = super::redacted(&response).unwrap().to_string();
        assert!(!written.contains(secret.expose()), "{written}");
        assert!(
            written.contains("request with <redacted> failed"),
            "{written}"
        );
        assert!(written.contains(r#"["nested","<redacted>"]"#), "{written}");
    }

    /// A daemon with a home holding instance `demo` whose default profile has one local mod.
    struct Fixture {
        _plans: TempDir,
        root: TempDir,
        daemon: Daemon,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with(&Web::default())
        }

        /// A fixture whose network is `web`.
        fn with(web: &Web) -> Self {
            let (plans, registry) = registry();
            let root = TempDir::new().unwrap();
            let daemon = Daemon::new(registry)
                .with_home(root.path().join("home"))
                .with_connector(web.connector());
            let mut fixture = Self {
                _plans: plans,
                root,
                daemon,
            };
            let game = fixture.root.path().join("game");
            fs::create_dir(&game).unwrap();
            let store = fixture.root.path().join("store");
            fixture.run(&[
                "instance",
                "add",
                "demo",
                "--root",
                game.to_str().unwrap(),
                "--game",
                "example",
                "--loader",
                "native",
                "--store",
                store.to_str().unwrap(),
            ]);
            fixture.add_mod("tool.txt");
            fixture
        }

        /// Starts browsers through `launcher`.
        fn with_browser(mut self, launcher: crate::Launcher) -> Self {
            let placeholder =
                Daemon::new(PlanRegistry::discover(PathBuf::from("missing-test-plans")).unwrap());
            self.daemon = std::mem::replace(&mut self.daemon, placeholder).with_browser(launcher);
            self
        }

        fn run(&mut self, args: &[&str]) {
            let result = success(call(
                &mut self.daemon,
                COMMAND_METHOD,
                json!({ "args": args }),
            ));
            assert_eq!(result.get("exit_code"), Some(&json!(0)), "{result}");
        }

        fn add_mod(&mut self, name: &str) {
            let path = self.root.path().join(name);
            fs::write(&path, name.as_bytes()).unwrap();
            self.run(&["add", "demo", path.to_str().unwrap()]);
        }

        fn output(&self) -> PathBuf {
            self.root.path().join("demo.msbepack")
        }

        fn home(&self) -> PathBuf {
            self.root.path().join("home")
        }

        fn preview_export(&mut self) -> (String, String) {
            let output = self.output();
            let preview = success(call(
                &mut self.daemon,
                PACK_EXPORT_PREVIEW_METHOD,
                json!({ "instance": "demo", "codec": "msbe-native", "output": output }),
            ));
            (
                text(&preview, "plan_id").to_owned(),
                text(&preview, "plan_digest").to_owned(),
            )
        }

        fn start(&mut self, method: &str, params: Value) -> Response {
            let request = serde_json::Map::from_iter([
                ("method".to_owned(), Value::from(method)),
                ("params".to_owned(), params),
            ]);
            call(&mut self.daemon, JOB_START_METHOD, Value::Object(request))
        }

        fn status(&mut self, job: u64) -> Value {
            success(call(
                &mut self.daemon,
                JOB_EVENTS_METHOD,
                json!({ "job_id": job }),
            ))
        }
    }

    fn pack_code(response: &Response) -> Option<&str> {
        match response {
            Response::Error { error, .. } if error.code == codes::PACK => error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str),
            _ => None,
        }
    }

    #[test]
    fn info_reports_the_current_contract() {
        let mut daemon =
            Daemon::new(PlanRegistry::discover(PathBuf::from("missing-test-plans")).unwrap());
        let result = success(call(&mut daemon, INFO_METHOD, Value::Null));
        assert_eq!(result.get("rpc_version"), Some(&json!(CONTRACT_VERSION)));
        assert!(result.get("data_directory").is_some_and(Value::is_string));
    }

    #[test]
    fn commands_are_executed_by_the_daemon() {
        let mut daemon =
            Daemon::new(PlanRegistry::discover(PathBuf::from("missing-test-plans")).unwrap());
        let result = success(call(
            &mut daemon,
            COMMAND_METHOD,
            json!({"args": ["--help"]}),
        ));
        assert_eq!(result.get("exit_code"), Some(&json!(0)));
        assert!(
            result
                .get("stdout")
                .and_then(Value::as_str)
                .is_some_and(|output| output.contains("msbe"))
        );
    }

    /// Canned JSON and files by URL, served to every network client the daemon opens. Once given
    /// the job queue, it checks that no download runs while the instance-state lock is held.
    #[derive(Clone, Default)]
    struct Web {
        json: BTreeMap<String, Value>,
        files: BTreeMap<String, Vec<u8>>,
        jobs: Arc<Mutex<Option<Arc<Jobs>>>>,
    }

    impl Web {
        fn connector(&self) -> Connector {
            let web = self.clone();
            Arc::new(move || -> Result<Box<dyn HttpClient>, HttpError> {
                Ok(Box::new(web.clone()))
            })
        }
    }

    impl HttpClient for Web {
        fn send(&self, request: &HttpRequest<'_>) -> Result<HttpResponse, HttpError> {
            self.json
                .get(request.url)
                .map(|body| serde_json::to_vec(body).unwrap().into())
                .ok_or_else(|| HttpError::Status {
                    url: request.url.to_owned(),
                    status: 404,
                })
        }

        fn download(
            &self,
            request: &HttpRequest<'_>,
            sink: &mut dyn Write,
        ) -> Result<u64, HttpError> {
            if let Some(jobs) = self.jobs.lock().unwrap().as_ref() {
                assert!(
                    jobs.try_state().is_ok(),
                    "downloads never hold the instance-state lock"
                );
            }
            let body = self
                .files
                .get(request.url)
                .ok_or_else(|| HttpError::Status {
                    url: request.url.to_owned(),
                    status: 404,
                })?;
            sink.write_all(body).unwrap();
            Ok(u64::try_from(body.len()).unwrap())
        }
    }

    /// A browser-assisted catalog for plan `example`, whose links use the `handoff` scheme.
    const ASSISTED: &str = r#"
runtime = "catalog-v1"
capabilities = ["project", "releases"]

[games]
example = "game"

[provider]
schema = 1
id = "assisted"
name = "Assisted"
[provider.source]
type = "prefixed"
prefix = "assisted:"
[provider.metadata]
api_base = "https://api.assisted.test"
[provider.acquisition]
type = "browser_assisted"
scheme = "handoff"
[provider.policy]
requires_auth = false
respects_distribution_flag = false
tos_url = ""
ack_required = false

[routes]
project = "/projects/{reference}"
releases = "/projects/{project}/releases"

[pages]
release = "https://www.assisted.test/{game}/mods/{project}?file={release}"

[handoff]
host = "game"
path = ["files", "{project}", "{release}"]
redeem = "/links/{project}/{release}"

[releases]
order = "newest-first"

[mappings.project]
id = "/id"
title = "/title"

[mappings.release]
id = "/id"
number = "/number"
published = "/published"
files = { single = "" }

[mappings.release.file]
url = "/url"
name = "/name"

[mappings.handoff]
urls = "/url"
"#;

    /// Installs [`ASSISTED`] as a signed program its signer is granted.
    fn install_assisted(home: &Path) {
        let key = SigningKey::from_bytes(&[7; 32]);
        let payload: ProviderProgram = toml::from_str(ASSISTED).unwrap();
        let mut envelope = ExtensionEnvelope {
            schema: 1,
            package_digest: ProviderProgramEnvelope::digest_for(&payload).unwrap(),
            id: payload.provider.id.clone(),
            version: "0.1.0".to_owned(),
            provides: vec![ExtensionProvide::ProviderProgramV1],
            host_api: HostApiRange {
                minimum: 1,
                maximum: 1,
            },
            capabilities: vec![ExtensionCapability::Network],
            signer: "publisher".to_owned(),
            signature: "00".repeat(64),
            payload,
        };
        envelope.sign(&key).unwrap();
        let programs = home.join("extensions/providers");
        fs::create_dir_all(&programs).unwrap();
        fs::write(
            programs.join("assisted.toml"),
            toml::to_string(&ProviderProgramEnvelope(envelope)).unwrap(),
        )
        .unwrap();
        fs::write(
            home.join("extensions/trust.toml"),
            format!(
                "[[signer]]\nid = \"publisher\"\nkey = \"{}\"\nprograms = [\"assisted\"]\n",
                hex(key.verifying_key().as_bytes())
            ),
        )
        .unwrap();
    }

    fn at<'a>(value: &'a Value, pointer: &str) -> &'a Value {
        value
            .pointer(pointer)
            .unwrap_or_else(|| panic!("no {pointer} in {value}"))
    }

    fn enqueue(daemon: &mut Daemon, source: &str) -> u64 {
        let item = success(call(
            daemon,
            DOWNLOAD_ENQUEUE_METHOD,
            json!({ "instance": "demo", "profile": "default", "source": source }),
        ));
        at(&item, "/id").as_u64().unwrap()
    }

    /// Download `id` as the queue lists it.
    fn item(daemon: &mut Daemon, id: u64) -> Value {
        let list = success(call(daemon, DOWNLOAD_LIST_METHOD, Value::Null));
        at(&list, "/items")
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item.get("id") == Some(&json!(id)))
            .cloned()
            .unwrap_or_else(|| panic!("no download {id} in {list}"))
    }

    fn state(daemon: &mut Daemon, id: u64) -> String {
        at(&item(daemon, id), "/state/kind")
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn refusal(response: &Response) -> &str {
        match response {
            Response::Error { error, .. } if error.code == -32602 => &error.message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn downloads_are_controlled_through_a_persisted_queue_read_as_a_cursor() {
        let mut fixture = Fixture::new();
        let daemon = &mut fixture.daemon;
        let first = enqueue(daemon, "https://files.example.test/one.txt");
        let second = enqueue(daemon, "https://files.example.test/two.txt");
        assert_eq!(
            enqueue(daemon, "https://files.example.test/one.txt"),
            first,
            "an unfinished source is queued once"
        );
        let listed = success(call(daemon, DOWNLOAD_LIST_METHOD, Value::Null));
        assert_eq!(at(&listed, "/order"), &json!([first, second]));
        let seen = at(&listed, "/next").as_u64().unwrap();
        let unchanged = success(call(daemon, DOWNLOAD_LIST_METHOD, json!({ "after": seen })));
        assert_eq!(at(&unchanged, "/items"), &json!([]));

        let paused = success(call(daemon, DOWNLOAD_PAUSE_METHOD, Value::Null));
        assert_eq!(at(&paused, "/paused"), &json!(true));
        assert!(!daemon.lanes().run_next(), "a paused queue starts nothing");
        success(call(daemon, DOWNLOAD_RESUME_METHOD, Value::Null));

        success(call(daemon, DOWNLOAD_PAUSE_METHOD, json!({ "id": second })));
        assert_eq!(state(daemon, second), "paused");
        let resumed = call(daemon, DOWNLOAD_RESUME_METHOD, json!({ "id": first }));
        assert!(refusal(&resumed).contains("cannot resume while queued"));
        let cancelled = success(call(
            daemon,
            DOWNLOAD_CANCEL_METHOD,
            json!({ "id": second }),
        ));
        assert_eq!(at(&cancelled, "/state/kind"), "cancelled");
        let retried = success(call(daemon, DOWNLOAD_RETRY_METHOD, json!({ "id": second })));
        assert_eq!(at(&retried, "/state/kind"), "queued");
        let moved = success(call(
            daemon,
            DOWNLOAD_MOVE_METHOD,
            json!({ "id": second, "position": 0 }),
        ));
        assert_eq!(at(&moved, "/order"), &json!([second, first]));
        let changed = success(call(daemon, DOWNLOAD_LIST_METHOD, json!({ "after": seen })));
        let changed: Vec<&Value> = at(&changed, "/items")
            .as_array()
            .unwrap()
            .iter()
            .map(|item| at(item, "/id"))
            .collect();
        assert_eq!(changed, [&json!(second)]);

        success(call(daemon, DOWNLOAD_CANCEL_METHOD, json!({ "id": first })));
        let cleared = success(call(daemon, DOWNLOAD_CLEAR_METHOD, Value::Null));
        assert_eq!(at(&cleared, "/order"), &json!([second]));

        let local = call(
            daemon,
            DOWNLOAD_ENQUEUE_METHOD,
            json!({ "instance": "demo", "profile": "default", "source": "/tmp/mod.txt" }),
        );
        assert!(refusal(&local).contains("msbe add"), "{local:?}");
        let unknown = call(
            daemon,
            DOWNLOAD_ENQUEUE_METHOD,
            json!({ "instance": "missing", "profile": "default", "source": "https://a.test/b" }),
        );
        assert!(refusal(&unknown).contains("no instance missing"));

        let (_plans, registry) = registry();
        let mut restarted = Daemon::new(registry).with_home(fixture.home());
        let restored = success(call(&mut restarted, DOWNLOAD_LIST_METHOD, Value::Null));
        assert_eq!(at(&restored, "/order"), &json!([second]));
    }

    #[test]
    fn a_queued_url_downloads_outside_the_state_lock_and_a_job_adds_it() {
        let url = "https://files.example.test/extra.txt";
        let mut web = Web::default();
        web.files.insert(url.to_owned(), b"extra".to_vec());
        let probe = Arc::clone(&web.jobs);
        let mut fixture = Fixture::with(&web);
        *probe.lock().unwrap() = Some(fixture.daemon.jobs());
        let id = enqueue(&mut fixture.daemon, url);

        assert!(fixture.daemon.lanes().run_next());
        assert_eq!(state(&mut fixture.daemon, id), "downloaded");
        let quarantine = fixture.home().join(format!("downloads/{id}"));
        assert!(quarantine.is_dir());
        assert!(fixture.daemon.jobs().run_next());
        let done = item(&mut fixture.daemon, id);
        assert_eq!(at(&done, "/state/kind"), "completed", "{done}");
        assert_eq!(at(&done, "/added"), &json!(["extra"]));
        assert!(!quarantine.exists(), "added files leave quarantine");
        let shown = success(call(
            &mut fixture.daemon,
            COMMAND_METHOD,
            json!({ "args": ["--format", "json", "profile", "show", "demo"] }),
        ));
        assert!(text(&shown, "stdout").contains("\"extra\""), "{shown}");

        let again = enqueue(&mut fixture.daemon, url);
        assert_ne!(again, id, "a finished source can be queued again");
        assert!(fixture.daemon.lanes().run_next());
        assert!(fixture.daemon.jobs().run_next());
        let skipped = item(&mut fixture.daemon, again);
        assert_eq!(at(&skipped, "/state/kind"), "completed", "{skipped}");
        assert_eq!(at(&skipped, "/skipped"), &json!(["extra"]));
    }

    #[test]
    fn an_assisted_file_waits_for_its_link_and_a_stray_link_waits_for_a_profile() {
        let mut web = Web::default();
        web.json.extend([
            (
                "https://api.assisted.test/projects/sprocket".to_owned(),
                json!({ "id": "sprocket", "title": "Sprocket" }),
            ),
            (
                "https://api.assisted.test/projects/sprocket/releases".to_owned(),
                json!([{
                    "id": "r1", "number": "1.0.0", "published": "2026-09-01",
                    "url": "https://files.assisted.test/sprocket.txt", "name": "sprocket.txt"
                }]),
            ),
            (
                "https://api.assisted.test/links/sprocket/r1".to_owned(),
                json!({ "url": "https://files.assisted.test/sprocket.txt" }),
            ),
            (
                "https://api.assisted.test/links/gear/r9".to_owned(),
                json!({ "url": "https://files.assisted.test/gear.txt" }),
            ),
        ]);
        web.files.extend([
            (
                "https://files.assisted.test/sprocket.txt".to_owned(),
                b"sprocket".to_vec(),
            ),
            (
                "https://files.assisted.test/gear.txt".to_owned(),
                b"gear".to_vec(),
            ),
        ]);
        let mut fixture = Fixture::with(&web);
        install_assisted(&fixture.home());
        let lanes = fixture.daemon.lanes();
        let id = enqueue(&mut fixture.daemon, "assisted:sprocket");

        assert!(lanes.run_next());
        let waiting = item(&mut fixture.daemon, id);
        assert_eq!(
            at(&waiting, "/state"),
            &json!({
                "kind": "awaiting_user",
                "page": "https://www.assisted.test/game/mods/sprocket?file=r1",
                "scheme": "handoff"
            }),
            "{waiting}"
        );
        let receipt = success(call(
            &mut fixture.daemon,
            HANDOFF_SUBMIT_METHOD,
            json!({ "uri": "handoff://game/files/sprocket/r1" }),
        ));
        assert_eq!(
            receipt,
            json!({
                "id": id, "provider": "assisted", "game": "example",
                "project": "sprocket", "release": "r1", "matched": true
            })
        );
        assert_eq!(state(&mut fixture.daemon, id), "downloading");
        assert!(lanes.run_next_link());
        assert_eq!(state(&mut fixture.daemon, id), "downloaded");
        assert!(fixture.daemon.jobs().run_next());
        let done = item(&mut fixture.daemon, id);
        assert_eq!(at(&done, "/state/kind"), "completed", "{done}");
        assert_eq!(at(&done, "/added"), &json!(["sprocket"]));

        let stray = success(call(
            &mut fixture.daemon,
            HANDOFF_SUBMIT_METHOD,
            json!({ "uri": "handoff://game/files/gear/r9" }),
        ));
        assert_eq!(at(&stray, "/matched"), &json!(false));
        let stray = at(&stray, "/id").as_u64().unwrap();
        assert!(lanes.run_next_link());
        let parked = item(&mut fixture.daemon, stray);
        assert_eq!(at(&parked, "/state/kind"), "downloaded", "{parked}");
        assert!(parked.get("target").is_none());
        assert!(
            !fixture.daemon.jobs().run_next(),
            "an item without a profile is never added"
        );
        success(call(
            &mut fixture.daemon,
            DOWNLOAD_CONFIRM_METHOD,
            json!({ "id": stray, "instance": "demo", "profile": "default" }),
        ));
        assert!(fixture.daemon.jobs().run_next());
        assert_eq!(
            at(&item(&mut fixture.daemon, stray), "/added"),
            &json!(["gear"])
        );
    }

    #[test]
    fn handoff_submission_refuses_invalid_input_without_echoing_link_keys() {
        let mut daemon =
            Daemon::new(PlanRegistry::discover(PathBuf::from("missing-test-plans")).unwrap());
        let invalid = call(
            &mut daemon,
            HANDOFF_SUBMIT_METHOD,
            json!({"unexpected": true}),
        );
        assert!(matches!(invalid, Response::Error { error, .. } if error.code == -32602));

        let refused = call(
            &mut daemon,
            HANDOFF_SUBMIT_METHOD,
            json!({"uri": "unknown://item?key=not-for-output"}),
        );
        assert!(matches!(refused, Response::Error { error, .. }
            if error.code == -32602 && !error.message.contains("not-for-output")));
    }

    type FakeLaunch = (
        msbe_browser_channel::Launch,
        std::io::PipeReader,
        std::io::PipeWriter,
    );

    /// A launcher whose browser is the test: each launch sends its arguments and the browser's ends
    /// of the channel.
    fn fake_browser() -> (crate::Launcher, std::sync::mpsc::Receiver<FakeLaunch>) {
        let (sender, receiver) = std::sync::mpsc::channel();
        let sender = Mutex::new(sender);
        let launcher: crate::Launcher = Arc::new(move |launch: &msbe_browser_channel::Launch| {
            let (daemon_reads, browser_writes) = std::io::pipe()?;
            let (browser_reads, daemon_writes) = std::io::pipe()?;
            sender
                .lock()
                .unwrap()
                .send((launch.clone(), browser_reads, browser_writes))
                .unwrap();
            Ok(crate::Process {
                reader: Box::new(daemon_reads),
                writer: Box::new(daemon_writes),
                stop: Box::new(|| {}),
            })
        });
        (launcher, receiver)
    }

    fn next_launch(started: &std::sync::mpsc::Receiver<FakeLaunch>) -> FakeLaunch {
        started
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the daemon started no browser")
    }

    /// The browser status once `done` holds, waiting for the daemon's channel thread.
    fn browser_when(daemon: &mut Daemon, done: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..1000 {
            let status = success(call(
                daemon,
                msbe_rpc_schema::BROWSER_STATUS_METHOD,
                Value::Null,
            ));
            if done(&status) {
                return status;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the browser never reached the expected state");
    }

    /// A fixture with `projects` from the assisted catalog waiting on their pages, whose browsers
    /// are started through `launcher`.
    fn assisted_waiting(projects: &[&str], launcher: crate::Launcher) -> (Fixture, Vec<u64>) {
        let mut web = Web::default();
        for project in projects {
            web.json.insert(
                format!("https://api.assisted.test/projects/{project}"),
                json!({ "id": project, "title": project }),
            );
            web.json.insert(
                format!("https://api.assisted.test/projects/{project}/releases"),
                json!([{
                    "id": "r1", "number": "1.0.0", "published": "2026-09-01",
                    "url": format!("https://files.assisted.test/{project}.txt"),
                    "name": format!("{project}.txt")
                }]),
            );
            web.json.insert(
                format!("https://api.assisted.test/links/{project}/r1"),
                json!({ "url": format!("https://files.assisted.test/{project}.txt") }),
            );
            web.files.insert(
                format!("https://files.assisted.test/{project}.txt"),
                project.as_bytes().to_vec(),
            );
        }
        let mut fixture = Fixture::with(&web).with_browser(launcher);
        install_assisted(&fixture.home());
        let ids = projects
            .iter()
            .map(|project| enqueue(&mut fixture.daemon, &format!("assisted:{project}")))
            .collect();
        let lanes = fixture.daemon.lanes();
        for _ in projects {
            assert!(lanes.run_next());
        }
        (fixture, ids)
    }

    /// Saves `bytes` in the browser's quarantine under a random name, as a page's download lands,
    /// reports it as `../sprocket.txt`, and returns its quarantine name.
    fn report_download(
        quarantine: &Path,
        reports: &mut std::io::PipeWriter,
        bytes: &[u8],
    ) -> String {
        let name = msbe_browser_channel::quarantine_name([7; 16]);
        fs::write(quarantine.join(&name), bytes).unwrap();
        msbe_browser_channel::write_frame(
            reports,
            &msbe_browser_channel::BrowserMessage::CapturedDownload {
                suggested_name: "../sprocket.txt".to_owned(),
                quarantine_file: name.clone(),
                origin_url: "https://cdn.assisted.test/sprocket.txt".to_owned(),
                size: u64::try_from(bytes.len()).unwrap(),
            },
        )
        .unwrap();
        name
    }

    fn page(project: &str) -> String {
        format!("https://www.assisted.test/game/mods/{project}?file=r1")
    }

    #[test]
    fn the_browser_captures_downloads_and_links_on_waiting_pages_and_advances() {
        use msbe_browser_channel::{BrowserMessage, DaemonMessage, read_frame, write_frame};

        let (launcher, started) = fake_browser();
        let (mut fixture, ids) = assisted_waiting(&["sprocket", "gear"], launcher);
        let [sprocket, gear] = ids.as_slice() else {
            panic!("two downloads were queued");
        };
        let daemon = &mut fixture.daemon;
        assert_eq!(
            success(call(
                daemon,
                msbe_rpc_schema::BROWSER_STATUS_METHOD,
                Value::Null
            )),
            json!({ "running": false, "waiting": 2, "auto_advance": false })
        );
        let missing = call(
            daemon,
            msbe_rpc_schema::BROWSER_OPEN_METHOD,
            json!({ "id": 99 }),
        );
        assert_eq!(refusal(&missing), "download 99 does not wait on a page");

        let opened = success(call(
            daemon,
            msbe_rpc_schema::BROWSER_OPEN_METHOD,
            json!({ "auto_advance": true }),
        ));
        let (launch, mut commands, mut reports) = next_launch(&started);
        let browser = fixture.root.path().join("home/browser/assisted");
        assert_eq!(launch.profile, browser.join("profile"));
        assert_eq!(launch.quarantine, browser.join("quarantine"));
        assert_eq!(
            launch
                .origins
                .iter()
                .map(msbe_browser_channel::Origin::as_str)
                .collect::<Vec<_>>(),
            ["https://www.assisted.test"]
        );
        assert_eq!(launch.schemes, ["handoff"]);
        assert_eq!(
            read_frame::<DaemonMessage>(&mut commands).unwrap(),
            Some(DaemonMessage::Navigate {
                url: page("sprocket")
            })
        );
        assert_eq!(
            opened,
            json!({
                "running": true, "provider": "assisted", "item": sprocket, "page": page("sprocket"),
                "position": 1, "waiting": 2, "auto_advance": true
            })
        );

        // The user downloads the file from the page, which the browser saves under a random name.
        write_frame(
            &mut reports,
            &BrowserMessage::NavigationState {
                url: page("sprocket"),
                title: "Sprocket files".to_owned(),
            },
        )
        .unwrap();
        let name = report_download(&launch.quarantine, &mut reports, b"sprocket bytes");
        assert_eq!(
            read_frame::<DaemonMessage>(&mut commands).unwrap(),
            Some(DaemonMessage::Navigate { url: page("gear") }),
            "auto-advance goes to the next waiting page"
        );
        let advanced = browser_when(daemon, |status| status.get("item") == Some(&json!(gear)));
        assert_eq!(at(&advanced, "/title"), "Sprocket files");
        assert_eq!(at(&advanced, "/position"), &json!(1));
        assert_eq!(at(&advanced, "/waiting"), &json!(1));
        let captured = item(daemon, *sprocket);
        assert_eq!(at(&captured, "/state/kind"), "downloaded", "{captured}");
        assert_eq!(at(&captured, "/files/0/name"), "sprocket.txt");
        assert_eq!(at(&captured, "/files/0/size"), &json!(14));
        assert!(!launch.quarantine.join(&name).exists());
        assert!(daemon.jobs().run_next());
        assert_eq!(state(daemon, *sprocket), "completed");

        // The next page hands over a link instead, which the queue redeems like any other.
        write_frame(
            &mut reports,
            &BrowserMessage::CapturedProtocolUrl {
                url: "handoff://game/files/gear/r1".to_owned(),
            },
        )
        .unwrap();
        let received = browser_when(daemon, |status| status.get("waiting") == Some(&json!(0)));
        assert!(received.get("item").is_none(), "{received}");
        assert!(daemon.lanes().run_next_link());
        assert_eq!(state(daemon, *gear), "downloaded");

        let closed = success(call(
            daemon,
            msbe_rpc_schema::BROWSER_CLOSE_METHOD,
            Value::Null,
        ));
        assert_eq!(at(&closed, "/running"), &json!(false));
        assert_eq!(
            read_frame::<DaemonMessage>(&mut commands).unwrap(),
            Some(DaemonMessage::Close)
        );
    }

    #[test]
    fn a_report_the_browser_may_not_make_ends_its_session_without_repeating_it() {
        type Report = fn(&mut std::io::PipeWriter);

        use std::io::Write as _;

        use msbe_browser_channel::{BrowserMessage, DaemonMessage, read_frame, write_frame};

        let (launcher, started) = fake_browser();
        let (mut fixture, ids) = assisted_waiting(&["sprocket"], launcher);
        let daemon = &mut fixture.daemon;
        let refusals: [(Report, &str); 3] = [
            (
                |pipe| {
                    write_frame(
                        pipe,
                        &BrowserMessage::CapturedDownload {
                            suggested_name: "queue.json".to_owned(),
                            quarantine_file: "../../downloads/queue.json".to_owned(),
                            origin_url: "https://cdn.assisted.test/x".to_owned(),
                            size: 1,
                        },
                    )
                    .unwrap();
                },
                "it reported a download outside its quarantine",
            ),
            (
                |pipe| {
                    write_frame(
                        pipe,
                        &BrowserMessage::CapturedProtocolUrl {
                            url: "other://game?key=not-for-output".to_owned(),
                        },
                    )
                    .unwrap();
                },
                "it reported a link in a scheme it does not capture",
            ),
            (
                |pipe| {
                    let body = br#"{"kind":"run_script","source":"key=not-for-output"}"#;
                    pipe.write_all(&u32::try_from(body.len()).unwrap().to_be_bytes())
                        .unwrap();
                    pipe.write_all(body).unwrap();
                },
                "a browser channel frame is not a message the channel carries",
            ),
        ];
        for (report, reason) in refusals {
            success(call(
                daemon,
                msbe_rpc_schema::BROWSER_OPEN_METHOD,
                Value::Null,
            ));
            let (_, mut commands, mut reports) = next_launch(&started);
            assert!(matches!(
                read_frame::<DaemonMessage>(&mut commands).unwrap(),
                Some(DaemonMessage::Navigate { .. })
            ));
            report(&mut reports);
            let stopped = browser_when(daemon, |status| {
                status.get("running") == Some(&json!(false))
            });
            assert_eq!(
                at(&stopped, "/message"),
                &json!(format!("the MSBE browser was stopped: {reason}")),
                "{stopped}"
            );
            assert!(!stopped.to_string().contains("not-for-output"));
            assert_eq!(
                read_frame::<DaemonMessage>(&mut commands).unwrap(),
                Some(DaemonMessage::Close)
            );
        }
        assert_eq!(state(daemon, *ids.first().unwrap()), "awaiting_user");
    }

    /// A tool program for plan `example`, whose game the tool calls `123456`.
    const TOOL_PROGRAM: &str = r#"
runtime = "tool-v1"
capabilities = []

[games]
example = "123456"

[provider]
schema = 1
id = "example-tool"
name = "Example tool"
[provider.source]
type = "prefixed"
prefix = "example-tool:"
[provider.acquisition]
type = "external_tool"
[provider.policy]
requires_auth = false
respects_distribution_flag = false
tos_url = "https://www.example.test/terms"
ack_required = true

[tool]
arguments = ["fetch", "--game", "{game}", "--item", "{item}", "--into", "{output}"]
output = ["content", "{game}", "{item}"]
timeout = 60
"#;

    /// Installs `document` as a signed program its signer is granted.
    fn install_program(home: &Path, document: &str) {
        let key = SigningKey::from_bytes(&[9; 32]);
        let payload: ProviderProgram = toml::from_str(document).unwrap();
        let id = payload.provider.id.clone();
        let mut envelope = ExtensionEnvelope {
            schema: 1,
            package_digest: ProviderProgramEnvelope::digest_for(&payload).unwrap(),
            id: id.clone(),
            version: "0.1.0".to_owned(),
            provides: vec![ExtensionProvide::ProviderProgramV1],
            host_api: HostApiRange {
                minimum: 1,
                maximum: 1,
            },
            capabilities: Vec::new(),
            signer: "publisher".to_owned(),
            signature: "00".repeat(64),
            payload,
        };
        envelope.sign(&key).unwrap();
        let programs = home.join("extensions/providers");
        fs::create_dir_all(&programs).unwrap();
        fs::write(
            programs.join(format!("{id}.toml")),
            toml::to_string(&ProviderProgramEnvelope(envelope)).unwrap(),
        )
        .unwrap();
        fs::write(
            home.join("extensions/trust.toml"),
            format!(
                "[[signer]]\nid = \"publisher\"\nkey = \"{}\"\nprograms = [\"{id}\"]\n",
                hex(key.verifying_key().as_bytes())
            ),
        )
        .unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn a_registered_tool_fetches_an_item_the_queue_adds_and_a_changed_one_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let mut fixture = Fixture::new();
        install_program(&fixture.home(), TOOL_PROGRAM);
        let program = fixture.root.path().join("bin/example-tool");
        fs::create_dir_all(program.parent().unwrap()).unwrap();
        fs::write(
            &program,
            "#!/bin/sh\nmkdir -p \"$7/content/$3/$5\" && printf 'fetched %s' \"$5\" > \"$7/content/$3/$5/item.txt\"\n",
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let daemon = &mut fixture.daemon;

        let unregistered = success(call(daemon, msbe_rpc_schema::TOOL_LIST_METHOD, Value::Null));
        assert_eq!(at(&unregistered, "/0/state"), "unregistered");
        let refused = call(
            daemon,
            msbe_rpc_schema::TOOL_REGISTER_METHOD,
            json!({ "provider": "example-tool", "program": program }),
        );
        assert!(refusal(&refused).contains("https://www.example.test/terms"));
        let registered = success(call(
            daemon,
            msbe_rpc_schema::TOOL_REGISTER_METHOD,
            json!({ "provider": "example-tool", "program": program, "accept_terms": true }),
        ));
        assert_eq!(at(&registered, "/state"), "registered");
        assert_eq!(
            at(&registered, "/sha256"),
            &json!(msbe_cli::program_sha256(&program).unwrap())
        );

        let id = enqueue(daemon, "example-tool:987");
        assert!(daemon.lanes().run_next());
        let fetched = item(daemon, id);
        assert_eq!(at(&fetched, "/state/kind"), "downloaded", "{fetched}");
        assert!(daemon.jobs().run_next());
        assert_eq!(state(daemon, id), "completed");
        assert_eq!(at(&item(daemon, id), "/added"), &json!(["987"]));

        fs::write(&program, "#!/bin/sh\necho replaced\n").unwrap();
        let changed = success(call(daemon, msbe_rpc_schema::TOOL_LIST_METHOD, Value::Null));
        assert_eq!(at(&changed, "/0/state"), "changed");
        let again = enqueue(daemon, "example-tool:654");
        assert!(daemon.lanes().run_next());
        let failed = item(daemon, again);
        assert_eq!(at(&failed, "/state/kind"), "failed", "{failed}");
        assert!(
            at(&failed, "/state/message")
                .as_str()
                .unwrap()
                .contains("changed after it was registered"),
            "{failed}"
        );

        let forgotten = success(call(
            daemon,
            msbe_rpc_schema::TOOL_FORGET_METHOD,
            json!({ "provider": "example-tool" }),
        ));
        assert_eq!(at(&forgotten, "/state"), "unregistered");
    }

    #[test]
    #[cfg(all(unix, not(target_os = "macos")))]
    fn handler_methods_ask_before_replacing_an_owner_and_give_the_scheme_back() {
        let (_plans, registry) = registry();
        let root = TempDir::new().unwrap();
        let home = root.path().join("home");
        install_assisted(&home);
        let program = root.path().join("bin/msbe");
        fs::create_dir_all(program.parent().unwrap()).unwrap();
        fs::write(&program, b"").unwrap();
        let data = root.path().join("data");
        fs::create_dir_all(data.join("applications")).unwrap();
        fs::write(
            data.join("applications/other.desktop"),
            "[Desktop Entry]\nName=Other\nExec=other %u\nMimeType=x-scheme-handler/handoff;\n",
        )
        .unwrap();
        let directories = msbe_core::config::Freedesktop {
            config_home: root.path().join("config"),
            config_dirs: Vec::new(),
            data_home: data,
            data_dirs: Vec::new(),
            desktops: Vec::new(),
        };
        let mut daemon = Daemon::new(registry).with_home(home).with_handlers(
            msbe_os_integration::Handlers::freedesktop(directories),
            program,
        );
        let other = json!({ "kind": "other", "name": "Other (other.desktop)" });

        let listed = success(call(
            &mut daemon,
            msbe_rpc_schema::HANDLER_STATUS_METHOD,
            Value::Null,
        ));
        assert_eq!(
            listed,
            json!([{ "scheme": "handoff", "provider": "assisted", "owner": other, "current": false }])
        );
        let refused = call(
            &mut daemon,
            msbe_rpc_schema::HANDLER_REGISTER_METHOD,
            json!({ "scheme": "handoff" }),
        );
        assert!(
            matches!(&refused, Response::Error { error, .. }
                if error.code == codes::HANDLER_OWNED
                    && error.data == Some(json!({ "scheme": "handoff", "owner": "Other (other.desktop)" }))),
            "{refused:?}"
        );
        let unclaimed = call(
            &mut daemon,
            msbe_rpc_schema::HANDLER_REGISTER_METHOD,
            json!({ "scheme": "elsewhere", "replace": true }),
        );
        assert!(matches!(unclaimed, Response::Error { error, .. } if error.code == -32602));

        let registered = success(call(
            &mut daemon,
            msbe_rpc_schema::HANDLER_REGISTER_METHOD,
            json!({ "scheme": "handoff", "replace": true }),
        ));
        assert_eq!(
            registered,
            json!({
                "scheme": "handoff", "provider": "assisted", "owner": { "kind": "msbe" },
                "current": true, "previous": "Other (other.desktop)"
            })
        );
        let released = success(call(
            &mut daemon,
            msbe_rpc_schema::HANDLER_UNREGISTER_METHOD,
            json!({ "scheme": "handoff" }),
        ));
        assert_eq!(at(&released, "/owner"), &other);
    }

    #[test]
    fn games_can_be_listed_unloaded_and_loaded_at_runtime() {
        let (_directory, registry) = registry();
        let mut daemon = Daemon::new(registry);
        let listed = success(call(&mut daemon, GAME_LIST_METHOD, Value::Null));
        assert_eq!(
            listed
                .as_array()
                .and_then(|games| games.first())
                .and_then(|game| game.get("name")),
            Some(&json!("Example Game"))
        );
        let unloaded = success(call(
            &mut daemon,
            PLAN_UNLOAD_METHOD,
            json!({"id": "example"}),
        ));
        assert_eq!(unloaded, json!({"unloaded": true}));
        let loaded = success(call(
            &mut daemon,
            PLAN_LOAD_METHOD,
            json!({"id": "example"}),
        ));
        assert_eq!(loaded.get("id"), Some(&json!("example")));
    }

    #[test]
    fn game_registration_is_resolved_only_while_loaded() {
        let (_directory, registry) = registry();
        let mut daemon = Daemon::new(registry);
        let params = || json!({"args": ["instance", "add", "demo", "--game", "example"]});
        assert!(matches!(
            call(&mut daemon, COMMAND_METHOD, params()),
            Response::Success { .. }
        ));
        let unloaded = success(call(
            &mut daemon,
            PLAN_UNLOAD_METHOD,
            json!({"id": "example"}),
        ));
        assert_eq!(unloaded, json!({"unloaded": true}));
        let rejected = call(&mut daemon, COMMAND_METHOD, params());
        assert!(matches!(rejected, Response::Error { error, .. } if error.code == -32010));
    }

    #[test]
    fn codecs_are_discovered_from_the_registry() {
        let mut daemon =
            Daemon::new(PlanRegistry::discover(PathBuf::from("missing-test-plans")).unwrap());
        let codecs = success(call(
            &mut daemon,
            PACK_CODEC_LIST_METHOD,
            json!({"direction": "export"}),
        ));
        let ids: Vec<&str> = codecs
            .as_array()
            .unwrap()
            .iter()
            .map(|codec| text(codec, "id"))
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|id| !id.is_empty()));
    }

    #[test]
    fn exports_execute_only_the_held_plan_and_only_as_a_job() {
        let mut fixture = Fixture::new();
        let (plan_id, digest) = fixture.preview_export();

        let direct = call(
            &mut fixture.daemon,
            PACK_EXPORT_EXECUTE_METHOD,
            json!({ "plan_id": plan_id, "plan_digest": digest }),
        );
        assert!(matches!(direct, Response::Error { error, .. } if error.code == -32600));
        let tampered = fixture.start(
            PACK_EXPORT_EXECUTE_METHOD,
            json!({ "plan_id": plan_id, "plan_digest": format!("sha256:{}", "0".repeat(64)) }),
        );
        assert_eq!(pack_code(&tampered), Some("StalePlan"));

        let started = success(fixture.start(
            PACK_EXPORT_EXECUTE_METHOD,
            json!({ "plan_id": plan_id, "plan_digest": digest }),
        ));
        let job = started.get("job_id").and_then(Value::as_u64).unwrap();
        assert_eq!(text(&fixture.status(job), "state"), "queued");
        assert!(fixture.daemon.jobs().run_next());
        let status = fixture.status(job);
        assert_eq!(text(&status, "state"), "succeeded", "{status}");
        assert!(fixture.output().is_file());

        let replayed = fixture.start(
            PACK_EXPORT_EXECUTE_METHOD,
            json!({ "plan_id": plan_id, "plan_digest": digest }),
        );
        assert_eq!(
            pack_code(&replayed),
            Some("StalePlan"),
            "a held plan runs once"
        );
    }

    #[test]
    fn a_plan_whose_profile_changed_fails_as_stale_when_its_job_runs() {
        let mut fixture = Fixture::new();
        let (plan_id, digest) = fixture.preview_export();
        fixture.add_mod("second.txt");
        let started = success(fixture.start(
            PACK_EXPORT_EXECUTE_METHOD,
            json!({ "plan_id": plan_id, "plan_digest": digest }),
        ));
        let job = started.get("job_id").and_then(Value::as_u64).unwrap();
        assert!(fixture.daemon.jobs().run_next());
        let status = fixture.status(job);
        assert_eq!(text(&status, "state"), "failed");
        let last = status
            .get("events")
            .and_then(Value::as_array)
            .and_then(|events| events.last())
            .unwrap();
        assert_eq!(text(last, "code"), "StalePlan");
        assert!(!fixture.output().exists());
    }

    #[test]
    fn queued_jobs_can_be_cancelled_before_they_run() {
        let mut fixture = Fixture::new();
        let output = fixture.root.path().join("demo.msbesnapshot");
        let started = success(fixture.start(
            SNAPSHOT_CREATE_METHOD,
            json!({ "instance": "demo", "output": output }),
        ));
        let job = started.get("job_id").and_then(Value::as_u64).unwrap();
        let cancelled = success(call(
            &mut fixture.daemon,
            JOB_CANCEL_METHOD,
            json!({ "job_id": job }),
        ));
        assert_eq!(cancelled, json!({ "cancelled": true }));
        assert!(!fixture.daemon.jobs().run_next());
        assert_eq!(text(&fixture.status(job), "state"), "cancelled");
        assert!(!output.exists());
    }
}
