//! The `msbe` command-line interface.
//!
//! See `docs/09-interfaces.md`.
#![expect(
    clippy::print_stdout,
    reason = "stdout is this binary's machine-readable interface; see docs/09-interfaces.md"
)]

use std::process::ExitCode;

fn main() -> ExitCode {
    println!(
        "msbe {} - scaffolding only; see docs/13-roadmap.md (M1).",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::SUCCESS
}
