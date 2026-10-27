//! `gap-conformance` command-line runner (`L5`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let code =
        gap_conformance::cli::run_cli_from_args(std::env::args_os(), &mut stdout, &mut stderr)
            .await;
    ExitCode::from(code)
}
