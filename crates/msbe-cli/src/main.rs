//! The `msbe` command-line interface.
//!
//! The CLI forwards commands to the local daemon, which is the sole mutation owner.
//!
//! The library remains directly callable for deterministic in-process tests. See
//! `docs/09-interfaces.md`.

use std::{
    ffi::OsString,
    io::{self, BufRead, Write},
    path::Path,
    process::{Command, ExitCode, Stdio},
};

use msbe_rpc_schema::{COMMAND_METHOD, Request};
use serde_json::{Value, json};

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    #[cfg(unix)]
    let code = match run_via_daemon(args) {
        Ok(code) => code,
        Err(error) => {
            drop(writeln!(io::stderr().lock(), "{error}"));
            1
        }
    };
    // Without a daemon to redact its responses, output is redacted on its way to the terminal.
    #[cfg(not(unix))]
    let code = msbe_cli::run(
        args,
        &mut msbe_secrets::redact::RedactingWriter::new(io::stdout().lock()),
        &mut msbe_secrets::redact::RedactingWriter::new(io::stderr().lock()),
    );
    ExitCode::from(code)
}

#[cfg(unix)]
fn run_via_daemon(args: Vec<OsString>) -> Result<u8, String> {
    use std::{os::unix::net::UnixStream, thread, time::Duration};

    let command = daemon_command(args)?;
    let socket = msbe_rpc_schema::default_socket();
    let mut stream = if let Ok(stream) = UnixStream::connect(&socket) {
        stream
    } else {
        start_daemon(&socket)?;
        connect_daemon(&socket, &thread::sleep, &Duration::from_millis(50))?
    };
    let request = Request::new(json!(1), COMMAND_METHOD, json!({"args": command}));
    serde_json::to_writer(&mut stream, &request).map_err(|error| error.to_string())?;
    stream.write_all(b"\n").map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())?;

    let mut response = String::new();
    io::BufReader::new(stream)
        .read_line(&mut response)
        .map_err(|error| error.to_string())?;
    let response: Value = serde_json::from_str(&response).map_err(|error| error.to_string())?;
    let result = response.get("result").ok_or_else(|| rpc_error(&response))?;
    let stdout = result
        .get("stdout")
        .and_then(Value::as_str)
        .ok_or_else(|| "daemon returned no stdout".to_owned())?;
    let stderr = result
        .get("stderr")
        .and_then(Value::as_str)
        .ok_or_else(|| "daemon returned no stderr".to_owned())?;
    io::stdout()
        .lock()
        .write_all(stdout.as_bytes())
        .map_err(|error| error.to_string())?;
    io::stderr()
        .lock()
        .write_all(stderr.as_bytes())
        .map_err(|error| error.to_string())?;
    let exit_code = result
        .get("exit_code")
        .and_then(Value::as_u64)
        .ok_or_else(|| "daemon returned no exit code".to_owned())?;
    u8::try_from(exit_code).map_err(|_| "daemon returned an invalid exit code".to_owned())
}

#[cfg(unix)]
fn daemon_command(args: Vec<OsString>) -> Result<Vec<String>, String> {
    let mut command = args
        .into_iter()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "arguments must be valid UTF-8".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if !command
        .iter()
        .any(|arg| arg == "--home" || arg.starts_with("--home="))
    {
        let home = msbe_core::config::Home::discover()
            .map_err(|error| error.to_string())?
            .root()
            .to_str()
            .ok_or_else(|| "MSBE_HOME must be valid UTF-8".to_owned())?
            .to_owned();
        command.splice(0..0, ["--home".to_owned(), home]);
    }
    Ok(command)
}

#[cfg(unix)]
fn start_daemon(socket: &Path) -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let daemon = executable.with_file_name("msbe-daemon");
    #[expect(
        clippy::disallowed_methods,
        reason = "the CLI must launch its local single-writer daemon when it is not running"
    )]
    Command::new(daemon)
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("cannot start msbe-daemon: {error}"))
}

#[cfg(unix)]
fn connect_daemon(
    socket: &Path,
    sleep: &dyn Fn(std::time::Duration),
    delay: &std::time::Duration,
) -> Result<std::os::unix::net::UnixStream, String> {
    for _ in 0..100 {
        match std::os::unix::net::UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(_) => sleep(*delay),
        }
    }
    Err(format!(
        "cannot connect to msbe-daemon at {}",
        socket.display()
    ))
}

#[cfg(unix)]
fn rpc_error(response: &Value) -> String {
    response
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("daemon returned an invalid response")
        .to_owned()
}
