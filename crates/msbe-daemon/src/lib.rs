//! JSON-RPC request handling for the MSBE daemon.

use std::io;

use msbe_rpc_schema::{
    COMMAND_METHOD, CONTRACT_VERSION, DaemonInfo, INFO_METHOD, Request, Response,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// Handles one fully decoded request.
///
/// Calls are synchronous by design at M1: the listener processes one request at a time, so
/// every filesystem mutation issued through this API has one owner.
pub fn handle(request: &Request) -> Response {
    if !request.is_versioned() {
        return Response::error(request.id.clone(), -32600, "expected JSON-RPC version 2.0");
    }
    match request.method.as_str() {
        INFO_METHOD => info(request.id.clone(), &request.params),
        COMMAND_METHOD => command(request.id.clone(), request.params.clone()),
        _ => Response::error(request.id.clone(), -32601, "method not found"),
    }
}

fn info(id: Value, params: &Value) -> Response {
    if !params.is_null() {
        return Response::error(id, -32602, "daemon.info does not accept parameters");
    }
    match serde_json::to_value(DaemonInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        rpc_version: CONTRACT_VERSION,
    }) {
        Ok(info) => Response::success(id, info),
        Err(error) => Response::error(id, -32603, error.to_string()),
    }
}

fn command(id: Value, params: Value) -> Response {
    let command = match serde_json::from_value::<Command>(params) {
        Ok(command) if !command.args.is_empty() => command,
        Ok(_) => return Response::error(id, -32602, "command.run requires at least one argument"),
        Err(error) => return Response::error(id, -32602, error.to_string()),
    };
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    args: Vec<String>,
}

/// Serves newline-delimited JSON-RPC over a Unix domain socket.
///
/// # Errors
///
/// Returns an error if the socket cannot be created, secured, or used to serve a client.
#[cfg(unix)]
pub fn serve(socket: &std::path::Path) -> io::Result<()> {
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
        serve_stream(stream?)?;
    }
    Ok(())
}

#[cfg(unix)]
fn serve_stream(stream: std::os::unix::net::UnixStream) -> io::Result<()> {
    use std::io::{BufRead, BufReader, Write};

    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line?;
        let response = match serde_json::from_str(&line) {
            Ok(request) => handle(&request),
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
    use serde_json::{Value, json};

    use super::{COMMAND_METHOD, INFO_METHOD, handle};
    use msbe_rpc_schema::{CONTRACT_VERSION, Request, Response};

    #[test]
    fn info_reports_the_current_contract() {
        let response = handle(&Request::new(json!(1), INFO_METHOD, Value::Null));
        match response {
            Response::Success { result, .. } => {
                assert_eq!(result.get("rpc_version"), Some(&json!(CONTRACT_VERSION)));
            }
            Response::Error { error, .. } => assert_eq!(error.code, 0, "{error:?}"),
        }
    }

    #[test]
    fn commands_are_executed_by_the_daemon() {
        let response = handle(&Request::new(
            json!(2),
            COMMAND_METHOD,
            json!({"args": ["--help"]}),
        ));
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
}
