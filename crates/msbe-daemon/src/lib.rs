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
use msbe_provider_api::{HttpClient, HttpError};
use msbe_rpc_schema::{
    COMMAND_METHOD, CONTRACT_VERSION, DOWNLOAD_CANCEL_METHOD, DOWNLOAD_CLEAR_METHOD,
    DOWNLOAD_CONFIRM_METHOD, DOWNLOAD_ENQUEUE_METHOD, DOWNLOAD_LIST_METHOD, DOWNLOAD_MOVE_METHOD,
    DOWNLOAD_PAUSE_METHOD, DOWNLOAD_RESUME_METHOD, DOWNLOAD_RETRY_METHOD, DaemonInfo,
    DownloadItemId, DownloadListRequest, DownloadPause, EXTENSION_LIST_METHOD, GAME_LIST_METHOD,
    HANDOFF_SUBMIT_METHOD, INFO_METHOD, JOB_CANCEL_METHOD, JOB_EVENTS_METHOD, JOB_METHODS,
    JOB_START_METHOD, PACK_CAPTURE_PREVIEW_METHOD, PACK_CODEC_LIST_METHOD,
    PACK_CODEC_OPTIONS_METHOD, PACK_EXPORT_PREVIEW_METHOD, PACK_IMPORT_PREVIEW_METHOD,
    PACK_UPDATE_PREVIEW_METHOD, PLAN_LOAD_METHOD, PLAN_UNLOAD_METHOD, Request, Response,
};
use msbe_secrets::SystemClock;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

mod downloads;
mod handoff;
mod jobs;
mod pack;
mod registry;

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
