//! The runner for external tools (`docs/06-providers-and-policy.md` §6.5). It runs the program a user
//! registered for a tool provider, and nothing else.
//!
//! The program must still have the SHA-256 it was registered with. It runs with its arguments as an
//! array and no shell, an environment holding only the search path, home directory and locale, fresh
//! working and output directories, and no standard input. What it prints is kept only up to a limit,
//! redacted, and never parsed. When it exits, times out, or its download is cancelled, every process
//! it started is stopped with it.

use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use msbe_core::config::Home;
use msbe_provider_api::{ToolArgument, ToolError, ToolHost, ToolInvocation};

/// How often a running tool is checked on.
const POLL: Duration = Duration::from_millis(50);
/// How many polls pass between checks for cancellation, which read the queue.
const CANCEL_POLLS: u32 = 20;
/// The most bytes of what a tool prints that are kept.
const OUTPUT_LIMIT: usize = 64 * 1024;
/// The most bytes of what a tool printed that a failure repeats.
const REPORTED: usize = 2048;

/// Runs registered tools for one download, which `cancelled` says whether the user cancelled.
pub(crate) struct ToolRunner<'a> {
    home: &'a Home,
    cancelled: &'a dyn Fn() -> bool,
}

impl<'a> ToolRunner<'a> {
    pub(crate) fn new(home: &'a Home, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self { home, cancelled }
    }

    fn wait(
        &self,
        child: &mut Child,
        invocation: &ToolInvocation,
    ) -> Result<ExitStatus, ToolError> {
        let tool = || invocation.tool.clone();
        let started = Instant::now();
        let mut polls = 0_u32;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => {}
                Err(error) => {
                    return Err(ToolError::Host {
                        tool: tool(),
                        message: error.to_string(),
                    });
                }
            }
            if started.elapsed() >= invocation.timeout {
                return Err(ToolError::TimedOut {
                    tool: tool(),
                    seconds: invocation.timeout.as_secs(),
                });
            }
            polls = polls.wrapping_add(1);
            if polls.is_multiple_of(CANCEL_POLLS) && (self.cancelled)() {
                return Err(ToolError::Cancelled { tool: tool() });
            }
            thread::sleep(POLL);
        }
    }
}

impl ToolHost for ToolRunner<'_> {
    fn run(&self, invocation: &ToolInvocation, dir: &Path) -> Result<PathBuf, ToolError> {
        let tool = || invocation.tool.clone();
        let host = |message: String| ToolError::Host {
            tool: tool(),
            message,
        };
        let registration = msbe_cli::tool_registration(self.home, &invocation.tool)
            .map_err(|error| host(error.to_string()))?
            .ok_or_else(|| ToolError::NotRegistered { tool: tool() })?;
        let found = msbe_cli::program_sha256(&registration.program).map_err(|error| {
            host(format!(
                "cannot read {}: {error}",
                registration.program.display()
            ))
        })?;
        if found != registration.sha256 {
            return Err(ToolError::Changed {
                tool: tool(),
                registered: registration.sha256,
                found,
            });
        }
        let output = dir.join("output");
        let work = dir.join("work");
        for directory in [&output, &work] {
            fs::create_dir(directory)
                .map_err(|error| host(format!("cannot create {}: {error}", directory.display())))?;
        }
        let arguments = invocation.arguments.iter().map(|argument| match argument {
            ToolArgument::Literal(text) => OsString::from(text),
            ToolArgument::Output => output.clone().into_os_string(),
        });
        #[expect(
            clippy::disallowed_methods,
            reason = "runs only the program the user registered for this tool, right after checking it still has the SHA-256 recorded when it was registered"
        )]
        let mut command = Command::new(&registration.program);
        command
            .args(arguments)
            .env_clear()
            .envs(msbe_core::config::tool_environment())
            .current_dir(&work)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command.spawn().map_err(|error| {
            host(format!(
                "cannot start {}: {error}",
                registration.program.display()
            ))
        })?;
        let captures = [
            child.stdout.take().map(capture),
            child.stderr.take().map(capture),
        ];
        let outcome = self.wait(&mut child, invocation);
        // Whatever the tool started is stopped too, even when the tool itself exited.
        stop(&mut child);
        let printed: Vec<u8> = captures
            .into_iter()
            .flatten()
            .flat_map(|capture| capture.join().unwrap_or_default())
            .collect();
        let status = outcome?;
        if status.success() {
            Ok(output)
        } else {
            Err(ToolError::Failed {
                tool: tool(),
                status: status.to_string(),
                output: report(&printed),
            })
        }
    }
}

/// Keeps the last [`OUTPUT_LIMIT`] bytes `reader` produces, on a thread of its own.
fn capture(mut reader: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    kept.extend_from_slice(buffer.get(..read).unwrap_or_default());
                    if kept.len() > OUTPUT_LIMIT {
                        let excess = kept.len() - OUTPUT_LIMIT;
                        kept.drain(..excess);
                    }
                }
            }
        }
        kept
    })
}

/// Stops `child` and, on Unix, every process in the group it leads.
fn stop(child: &mut Child) {
    #[cfg(unix)]
    if let Some(group) = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _stopped = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    drop(child.kill());
    drop(child.wait());
}

/// The end of what a tool printed, redacted, as a line of its own; empty when it printed nothing.
fn report(printed: &[u8]) -> String {
    let text = String::from_utf8_lossy(printed);
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let start = text
        .char_indices()
        .map(|(index, _)| index)
        .find(|index| text.len() - index <= REPORTED)
        .unwrap_or(0);
    format!(
        "\n{}",
        msbe_secrets::redact::redact(text.get(start..).unwrap_or_default())
    )
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        path::{Path, PathBuf},
        time::{Duration, Instant},
    };

    use msbe_core::config::Home;
    use msbe_provider_api::{ToolArgument, ToolError, ToolHost, ToolInvocation};
    use tempfile::TempDir;

    use super::ToolRunner;

    const TOOL: &str = "example-tool";

    /// A data directory with `body` registered as the program for [`TOOL`].
    struct Fixture {
        root: TempDir,
        home: Home,
        program: PathBuf,
    }

    impl Fixture {
        fn new(body: &str) -> Self {
            let root = TempDir::new().unwrap();
            let program = root.path().join("bin/tool");
            fs::create_dir_all(program.parent().unwrap()).unwrap();
            fs::write(&program, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
            let home = Home::at(root.path().join("home"));
            let tools = msbe_cli::tools_file(&home);
            fs::create_dir_all(tools.parent().unwrap()).unwrap();
            fs::write(
                &tools,
                format!(
                    "[[tool]]\nprovider = \"{TOOL}\"\nprogram = \"{}\"\nsha256 = \"{}\"\nregistered = 0\n",
                    program.display(),
                    msbe_cli::program_sha256(&program).unwrap()
                ),
            )
            .unwrap();
            Self {
                root,
                home,
                program,
            }
        }

        /// Runs the tool with `fetch <output>` in a fresh directory.
        fn run(
            &self,
            timeout: Duration,
            cancelled: &dyn Fn() -> bool,
        ) -> (PathBuf, Result<PathBuf, ToolError>) {
            let dir = self.root.path().join(format!(
                "slot-{}",
                fs::read_dir(self.root.path()).unwrap().count()
            ));
            fs::create_dir(&dir).unwrap();
            let invocation = ToolInvocation {
                tool: TOOL.to_owned(),
                arguments: vec![
                    ToolArgument::Literal("fetch".to_owned()),
                    ToolArgument::Output,
                ],
                output: Vec::new(),
                timeout,
            };
            let result = ToolRunner::new(&self.home, cancelled).run(&invocation, &dir);
            (dir, result)
        }
    }

    const NEVER: &dyn Fn() -> bool = &|| false;

    #[test]
    fn a_registered_tool_runs_with_a_scrubbed_environment_in_fresh_directories() {
        let fixture = Fixture::new(
            r#"env > "$2/env.txt"; pwd > "$2/pwd.txt"; cat > "$2/stdin.txt"; echo "$1" > "$2/argument.txt""#,
        );
        let (dir, result) = fixture.run(Duration::from_secs(20), NEVER);
        let output = result.unwrap();
        assert_eq!(output, dir.join("output"));
        let read = |name: &str| fs::read_to_string(output.join(name)).unwrap();
        assert_eq!(read("argument.txt"), "fetch\n");
        assert_eq!(read("stdin.txt"), "");
        assert_eq!(read("pwd.txt").trim(), dir.join("work").to_str().unwrap());
        let allowed = [
            "PATH", "HOME", "LANG", "LANGUAGE", "PWD", "OLDPWD", "SHLVL", "_",
        ];
        for line in read("env.txt").lines() {
            let name = line.split('=').next().unwrap();
            assert!(
                allowed.contains(&name) || name.starts_with("LC_"),
                "the tool received {name}"
            );
        }
    }

    #[test]
    fn a_program_that_changed_or_was_never_registered_does_not_run() {
        let fixture = Fixture::new(r#"touch "$2/ran""#);
        fs::write(
            &fixture.program,
            "#!/bin/sh\ntouch \"$2/ran\"; echo changed\n",
        )
        .unwrap();
        let (dir, result) = fixture.run(Duration::from_secs(20), NEVER);
        assert!(
            matches!(result, Err(ToolError::Changed { .. })),
            "{result:?}"
        );
        assert!(!dir.join("output/ran").exists());

        fs::write(msbe_cli::tools_file(&fixture.home), "").unwrap();
        let (_, result) = fixture.run(Duration::from_secs(20), NEVER);
        assert!(
            matches!(result, Err(ToolError::NotRegistered { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_failing_tool_reports_the_end_of_what_it_printed() {
        let fixture = Fixture::new("echo 'sign in with the tool first' >&2; exit 3");
        let (_, result) = fixture.run(Duration::from_secs(20), NEVER);
        let Err(ToolError::Failed { status, output, .. }) = result else {
            panic!("expected a failure, got {result:?}");
        };
        assert!(status.contains('3'), "{status}");
        assert_eq!(output, "\nsign in with the tool first");
    }

    /// Whether process `pid` is gone within a few seconds.
    fn gone(pid: &Path) -> bool {
        let pid: i32 = fs::read_to_string(pid).unwrap().trim().parse().unwrap();
        let pid = rustix::process::Pid::from_raw(pid).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if rustix::process::test_kill_process(pid).is_err() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn a_tool_past_its_timeout_is_stopped_with_every_process_it_started() {
        let fixture = Fixture::new(r#"sleep 30 & echo $! > "$2/child"; wait"#);
        let started = Instant::now();
        let (dir, result) = fixture.run(Duration::from_secs(1), NEVER);
        assert!(
            matches!(result, Err(ToolError::TimedOut { seconds: 1, .. })),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(
            gone(&dir.join("output/child")),
            "the tool's child still runs"
        );
    }

    #[test]
    fn cancelling_the_download_stops_the_tool_with_every_process_it_started() {
        let fixture = Fixture::new(r#"sleep 30 & echo $! > "$2/child"; wait"#);
        let (dir, result) = fixture.run(Duration::from_secs(60), &|| true);
        assert!(
            matches!(result, Err(ToolError::Cancelled { .. })),
            "{result:?}"
        );
        assert!(
            gone(&dir.join("output/child")),
            "the tool's child still runs"
        );
    }
}
