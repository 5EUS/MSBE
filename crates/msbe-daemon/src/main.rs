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
    let options = match arguments(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    #[cfg(unix)]
    match msbe_daemon::serve(&options.socket, &options.plans, options.home) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cannot serve {}: {error}", options.socket.display());
            ExitCode::from(1)
        }
    }
    #[cfg(not(unix))]
    {
        eprintln!("msbe-daemon local sockets are not yet implemented on this platform");
        ExitCode::from(1)
    }
}

struct Options {
    socket: PathBuf,
    plans: PathBuf,
    home: Option<PathBuf>,
}

fn arguments(args: &[std::ffi::OsString]) -> Result<Options, String> {
    let mut socket = default_socket();
    let mut plans = default_plans()?;
    let mut home = None;
    let mut arguments = args.iter();
    while let Some(flag) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| format!("{} requires a path", flag.to_string_lossy()))?;
        match flag.to_str() {
            Some("--socket") => socket = PathBuf::from(value),
            Some("--plans") => plans = PathBuf::from(value),
            Some("--home") => home = Some(PathBuf::from(value)),
            _ => {
                return Err(
                    "usage: msbe-daemon [--socket PATH] [--plans DIR] [--home DIR]".to_owned(),
                );
            }
        }
    }
    Ok(Options {
        socket,
        plans,
        home,
    })
}

fn default_socket() -> PathBuf {
    msbe_rpc_schema::default_socket()
}

fn default_plans() -> Result<PathBuf, String> {
    env::current_dir()
        .map(|directory| directory.join("plans"))
        .map_err(|error| format!("cannot determine run directory: {error}"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::arguments;

    #[test]
    fn accepts_a_custom_socket() {
        let args = vec!["--socket".into(), "/tmp/msbe-test.sock".into()];
        let options = arguments(&args).unwrap();
        assert_eq!(options.socket, PathBuf::from("/tmp/msbe-test.sock"));
    }

    #[test]
    fn accepts_a_custom_plan_registry() {
        let args = vec!["--plans".into(), "/tmp/msbe-plans".into()];
        let options = arguments(&args).unwrap();
        assert_eq!(options.plans, PathBuf::from("/tmp/msbe-plans"));
    }
}
