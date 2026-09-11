//! Isolated browser host process.
//!
//! Runs in its own process with no IPC binding into the app. Talks to the daemon over
//! a narrow, one-way, typed capture channel carrying only protocol URLs, quarantined
//! downloads and navigation state.
//!
//! See `docs/07-browser-and-secrets.md`.
#![expect(
    clippy::print_stderr,
    reason = "this process is launched by the daemon and reports startup failures on stderr"
)]

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!(
        "msbe-browser {} - scaffolding only; see docs/13-roadmap.md (M5).",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::SUCCESS
}
