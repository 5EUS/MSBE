//! The MSBE daemon: JSON-RPC server and the single writer for all mutation.
//!
//! See `docs/03-architecture.md`.
#![expect(
    clippy::print_stderr,
    reason = "the daemon has no logger configured before startup completes"
)]

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!(
        "msbe-daemon {} - scaffolding only; see docs/13-roadmap.md (M1).",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::SUCCESS
}
