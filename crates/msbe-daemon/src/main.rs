//! The MSBE daemon executable.
//!
//! See `docs/03-architecture.md`.
#![expect(
    clippy::print_stderr,
    reason = "the daemon has no logger configured before startup completes"
)]

use std::{env, path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = env::args_os().skip(1).collect();
    let socket = match socket_argument(&args) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    #[cfg(unix)]
    match msbe_daemon::serve(&socket) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cannot serve {}: {error}", socket.display());
            ExitCode::from(1)
        }
    }
    #[cfg(not(unix))]
    {
        eprintln!("msbe-daemon local sockets are not yet implemented on this platform");
        ExitCode::from(1)
    }
}

fn socket_argument(args: &[std::ffi::OsString]) -> Result<PathBuf, String> {
    match args {
        [] => Ok(default_socket()),
        [flag, path] if flag == "--socket" => Ok(PathBuf::from(path)),
        _ => Err("usage: msbe-daemon [--socket PATH]".to_owned()),
    }
}

fn default_socket() -> PathBuf {
    msbe_rpc_schema::default_socket()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::socket_argument;

    #[test]
    fn accepts_a_custom_socket() {
        let args = vec!["--socket".into(), "/tmp/msbe-test.sock".into()];
        let socket = socket_argument(&args);
        assert_eq!(socket, Ok(PathBuf::from("/tmp/msbe-test.sock")));
    }
}
