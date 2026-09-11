//! The `msbe` command-line interface.
//!
//! All behaviour lives in the `msbe_cli` library; this only connects the real arguments,
//! stdout and stderr. See `docs/09-interfaces.md`.

use std::{io, process::ExitCode};

fn main() -> ExitCode {
    let code = msbe_cli::run(
        std::env::args_os(),
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    );
    ExitCode::from(code)
}
