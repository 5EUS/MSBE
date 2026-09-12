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
    COMMAND_METHOD, CONTRACT_VERSION, DaemonInfo, GAME_LIST_METHOD, INFO_METHOD, JOB_CANCEL_METHOD,
    JOB_EVENTS_METHOD, JOB_METHODS, JOB_START_METHOD, PACK_CAPTURE_PREVIEW_METHOD,
    PACK_CODEC_LIST_METHOD, PACK_CODEC_OPTIONS_METHOD, PACK_EXPORT_PREVIEW_METHOD,
    PACK_IMPORT_PREVIEW_METHOD, PACK_UPDATE_PREVIEW_METHOD, PLAN_LOAD_METHOD, PLAN_UNLOAD_METHOD,
    Request, Response,
};
use serde::Deserialize;
use serde_json::{Value, json};

mod jobs;
mod pack;
mod registry;

pub use jobs::Jobs;
pub use registry::{Game, PlanRegistry, RegistryError};

/// Opens a network client for a job that acquires content.
pub type Connector = Arc<dyn Fn() -> Result<Box<dyn HttpClient>, HttpError> + Send + Sync>;

/// The daemon's state: loaded game support, previews awaiting execution, and the job queue.
pub struct Daemon {
    registry: PlanRegistry,
    home: Option<PathBuf>,
    plans: pack::Plans,
    jobs: Arc<Jobs>,
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

    /// Handles one fully decoded request.
    pub fn handle(&mut self, request: &Request) -> Response {
        if !request.is_versioned() {
            return Response::error(request.id.clone(), -32600, "expected JSON-RPC version 2.0");
        }
        let id = request.id.clone();
        match request.method.as_str() {
            INFO_METHOD => self.info(id, &request.params),
            COMMAND_METHOD => self.command(id, request.params.clone()),
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
                },
            );
            json!({ "job_id": job })
        });
        pack::respond(id, result)
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
        serde_json::to_writer(&mut writer, &response).map_err(io::Error::other)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::{
        COMMAND_METHOD, Daemon, GAME_LIST_METHOD, INFO_METHOD, PLAN_UNLOAD_METHOD, PlanRegistry,
    };
    use msbe_rpc_schema::{
        CONTRACT_VERSION, JOB_CANCEL_METHOD, JOB_EVENTS_METHOD, JOB_START_METHOD,
        PACK_CODEC_LIST_METHOD, PACK_EXPORT_EXECUTE_METHOD, PACK_EXPORT_PREVIEW_METHOD,
        PLAN_LOAD_METHOD, Request, Response, SNAPSHOT_CREATE_METHOD, codes,
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

    /// A daemon with a home holding instance `demo` whose default profile has one local mod.
    struct Fixture {
        _plans: TempDir,
        root: TempDir,
        daemon: Daemon,
    }

    impl Fixture {
        fn new() -> Self {
            let (plans, registry) = registry();
            let root = TempDir::new().unwrap();
            let daemon = Daemon::new(registry).with_home(root.path().join("home"));
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
