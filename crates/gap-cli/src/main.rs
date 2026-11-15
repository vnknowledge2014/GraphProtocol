//! `gap` command-line interface binary entry point (`L6`, `OPS-044`).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

#[tokio::main]
async fn main() {
    let code = gap_cli::commands::run_from_env().await;
    if code != 0 {
        std::process::exit(code);
    }
}
