//! JSON-RPC request handling for the MSBE daemon.

use std::{io, path::Path};

use msbe_rpc_schema::{
    COMMAND_METHOD, CONTRACT_VERSION, DaemonInfo, GAME_LIST_METHOD, INFO_METHOD, PLAN_LOAD_METHOD,
    PLAN_UNLOAD_METHOD, Request, Response,
};
use serde::Deserialize;
use serde_json::{Value, json};

mod registry;

pub use registry::{Game, PlanRegistry, RegistryError};

/// Handles one fully decoded request.
///
/// Calls are synchronous by design at M1: the listener processes one request at a time, so
/// every filesystem mutation issued through this API has one owner.
pub fn handle(request: &Request, registry: &mut PlanRegistry) -> Response {
    if !request.is_versioned() {
        return Response::error(request.id.clone(), -32600, "expected JSON-RPC version 2.0");
    }
    match request.method.as_str() {
        INFO_METHOD => info(request.id.clone(), &request.params),
        COMMAND_METHOD => command(request.id.clone(), request.params.clone(), registry),
        GAME_LIST_METHOD => game_list(request.id.clone(), &request.params, registry),
        PLAN_LOAD_METHOD => plan_load(request.id.clone(), request.params.clone(), registry),
        PLAN_UNLOAD_METHOD => plan_unload(request.id.clone(), request.params.clone(), registry),
        _ => Response::error(request.id.clone(), -32601, "method not found"),
    }
}

fn info(id: Value, params: &Value) -> Response {
    if !params.is_null() {
        return Response::error(id, -32602, "daemon.info does not accept parameters");
    }
    // Commands run without `--home`, so the discovered home is the one they use.
    let data_directory = msbe_core::config::Home::discover()
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

fn command(id: Value, params: Value, registry: &PlanRegistry) -> Response {
    let mut command = match serde_json::from_value::<Command>(params) {
        Ok(command) if !command.args.is_empty() => command,
        Ok(_) => return Response::error(id, -32602, "command.run requires at least one argument"),
        Err(error) => return Response::error(id, -32602, error.to_string()),
    };
    if let Err(message) = resolve_game(&mut command.args, registry) {
        return Response::error(id, -32010, message);
    }
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();
    let mut args = Vec::with_capacity(command.args.len() + 1);
    args.push("msbe".to_owned());
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

/// Serves newline-delimited JSON-RPC over a Unix domain socket.
///
/// # Errors
///
/// Returns an error if the socket cannot be created, secured, or used to serve a client.
#[cfg(unix)]
pub fn serve(socket: &Path, plans: &Path) -> io::Result<()> {
    use std::{
        fs,
        os::{
            unix::fs::PermissionsExt,
            unix::net::{UnixListener, UnixStream},
        },
    };

    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut registry = PlanRegistry::discover(plans.to_owned()).map_err(io::Error::other)?;
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
    for stream in listener.incoming() {
        serve_stream(stream?, &mut registry)?;
    }
    Ok(())
}

#[cfg(unix)]
fn serve_stream(
    stream: std::os::unix::net::UnixStream,
    registry: &mut PlanRegistry,
) -> io::Result<()> {
    use std::io::{BufRead, BufReader, Write};

    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        let response = match serde_json::from_str(&line) {
            Ok(request) => handle(&request, registry),
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
        COMMAND_METHOD, GAME_LIST_METHOD, INFO_METHOD, PLAN_UNLOAD_METHOD, PlanRegistry, handle,
    };
    use msbe_rpc_schema::{CONTRACT_VERSION, PLAN_LOAD_METHOD, Request, Response};

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

    #[test]
    fn info_reports_the_current_contract() {
        let mut registry = PlanRegistry::discover(PathBuf::from("missing-test-plans")).unwrap();
        let response = handle(
            &Request::new(json!(1), INFO_METHOD, Value::Null),
            &mut registry,
        );
        match response {
            Response::Success { result, .. } => {
                assert_eq!(result.get("rpc_version"), Some(&json!(CONTRACT_VERSION)));
                assert!(result.get("data_directory").is_some_and(Value::is_string));
            }
            Response::Error { error, .. } => assert_eq!(error.code, 0, "{error:?}"),
        }
    }

    #[test]
    fn commands_are_executed_by_the_daemon() {
        let mut registry = PlanRegistry::discover(PathBuf::from("missing-test-plans")).unwrap();
        let response = handle(
            &Request::new(json!(2), COMMAND_METHOD, json!({"args": ["--help"]})),
            &mut registry,
        );
        match response {
            Response::Success { result, .. } => {
                assert_eq!(result.get("exit_code"), Some(&json!(0)));
                assert!(
                    result
                        .get("stdout")
                        .and_then(Value::as_str)
                        .is_some_and(|output| output.contains("msbe"))
                );
            }
            Response::Error { error, .. } => assert_eq!(error.code, 0, "{error:?}"),
        }
    }

    #[test]
    fn games_can_be_listed_unloaded_and_loaded_at_runtime() {
        let (_directory, mut registry) = registry();
        let listed = handle(
            &Request::new(json!(3), GAME_LIST_METHOD, Value::Null),
            &mut registry,
        );
        match listed {
            Response::Success { result, .. } => {
                assert_eq!(
                    result
                        .as_array()
                        .and_then(|games| games.first())
                        .and_then(|game| game.get("name")),
                    Some(&json!("Example Game"))
                );
            }
            Response::Error { error, .. } => assert_eq!(error.code, 0, "{error:?}"),
        }

        let unloaded = handle(
            &Request::new(json!(4), PLAN_UNLOAD_METHOD, json!({"id": "example"})),
            &mut registry,
        );
        assert!(
            matches!(unloaded, Response::Success { result, .. } if result == json!({"unloaded": true}))
        );
        assert!(registry.games().is_empty());

        let loaded = handle(
            &Request::new(json!(5), PLAN_LOAD_METHOD, json!({"id": "example"})),
            &mut registry,
        );
        assert!(
            matches!(loaded, Response::Success { result, .. } if result.get("id") == Some(&json!("example")))
        );
    }

    #[test]
    fn game_registration_is_resolved_only_while_loaded() {
        let (_directory, mut registry) = registry();
        let request = || {
            Request::new(
                json!(6),
                COMMAND_METHOD,
                json!({"args": ["instance", "add", "demo", "--game", "example"]}),
            )
        };
        let resolved = handle(&request(), &mut registry);
        assert!(matches!(resolved, Response::Success { .. }));

        assert!(registry.unload("example"));
        let rejected = handle(&request(), &mut registry);
        assert!(matches!(rejected, Response::Error { error, .. } if error.code == -32010));
    }
}
