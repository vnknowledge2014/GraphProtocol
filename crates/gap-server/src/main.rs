//! `gap-server` binary entry point (`WP-M1-12`: thin wrapper around [`gap_server::run_server_from_path`]).
#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented
)]

use std::ffi::OsString;
use std::io::{self, Write as _};
use std::path::PathBuf;

use gap_server::{EXIT_CONFIG_ERROR, EXIT_OK, run_server_from_path};

const USAGE: &str = "\
Usage: gap-server [--config <FILE>] [--stdio]

Options:
      --config <FILE>  Path to gap.toml [default: gap.toml]
      --stdio          Serve MCP JSON-RPC over stdin/stdout instead of HTTP
  -h, --help           Print help
  -V, --version        Print version
";

enum ParsedArgs {
    Help,
    Version,
    Run { config: PathBuf, stdio: bool },
}

fn parse_args<I: IntoIterator<Item = OsString>>(args: I) -> Result<ParsedArgs, String> {
    let mut iter = args.into_iter();
    let _program = iter.next();
    let mut config = PathBuf::from("gap.toml");
    let mut stdio = false;

    while let Some(arg) = iter.next() {
        let Some(flag) = arg.to_str() else {
            return Err("non-UTF-8 CLI flag".to_owned());
        };
        match flag {
            "-h" | "--help" => return Ok(ParsedArgs::Help),
            "-V" | "--version" => return Ok(ParsedArgs::Version),
            "--stdio" => stdio = true,
            "--config" | "-c" => {
                let Some(val) = iter.next() else {
                    return Err("missing value for `--config <FILE>`".to_owned());
                };
                config = PathBuf::from(val);
            }
            _ if flag.starts_with("--config=") => {
                let val = flag.trim_start_matches("--config=");
                if val.is_empty() {
                    return Err("empty value for `--config=<FILE>`".to_owned());
                }
                config = PathBuf::from(val);
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }

    Ok(ParsedArgs::Run { config, stdio })
}

#[tokio::main]
async fn main() {
    let code = match parse_args(std::env::args_os()) {
        Ok(ParsedArgs::Help) => {
            let _ = io::stdout().write_all(USAGE.as_bytes());
            EXIT_OK
        }
        Ok(ParsedArgs::Version) => {
            let _ = writeln!(io::stdout(), "gap-server {}", env!("CARGO_PKG_VERSION"));
            EXIT_OK
        }
        Ok(ParsedArgs::Run { config, stdio }) => run_server_from_path(&config, stdio).await,
        Err(msg) => {
            let _ = writeln!(io::stderr(), "error: {msg}\n\n{USAGE}");
            EXIT_CONFIG_ERROR
        }
    };
    if code != EXIT_OK {
        std::process::exit(code);
    }
}
