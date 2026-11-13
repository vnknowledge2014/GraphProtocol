//! `clap`-derived `OPS-044` command tree and in-process dispatcher (`Brief §3.4`).

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::error::ErrorKind;
use clap::{Parser, Subcommand};
use gap_core::config::{GapConfig, Profile};
use gap_server::{EXIT_CONFIG_ERROR, EXIT_OK, ServerFile, run_server_from_path};

use crate::doctor::run_doctor;
use crate::init::run_init;

/// `gap` — Graph Agent Protocol gateway and administrative CLI (`OPS-044`).
#[derive(Debug, Parser)]
#[command(
    name = "gap",
    version,
    about = "Graph Agent Protocol (GAP) gateway and administrative CLI"
)]
pub struct Cli {
    /// Top-level `OPS-044` subcommand.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level `OPS-044` command inventory.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Assemble the configured supergraph and serve HTTP or `stdio` MCP (`OPS-001`, `OPS-002`).
    Serve {
        /// Path to `gap.toml`.
        #[arg(long, default_value = "gap.toml")]
        config: PathBuf,
        /// Serve MCP JSON-RPC over `stdin`/`stdout` instead of HTTP (`SEC-004`).
        #[arg(long)]
        stdio: bool,
    },
    /// Scaffold `gap.toml` and the walking-skeleton fixtures into `DIR` (`OPS-044`).
    Init {
        /// Target workspace directory.
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Overwrite existing regular files inside `DIR` (never follows symlinks).
        #[arg(long)]
        force: bool,
    },
    /// Run configuration, credential-posture, and branch diagnostics (`OPS-044`, `SEC-072`).
    Doctor {
        /// Path to `gap.toml`.
        #[arg(long, default_value = "gap.toml")]
        config: PathBuf,
    },
    /// Inspect the `gap.toml` JSON Schema or validate a configuration file (`OPS-044`).
    Config {
        /// `config` subcommand.
        #[command(subcommand)]
        action: ConfigSubcommand,
    },
    /// Manage local System-1 / embedding model artifacts (`WP-M2-04`).
    Models {
        /// `models` subcommand.
        #[command(subcommand)]
        action: ModelsSubcommand,
    },
    /// Review and decide ingestion proposals (`WP-M4-07`).
    Proposals {
        /// `proposals` subcommand.
        #[command(subcommand)]
        action: ProposalsSubcommand,
    },
    /// Inspect, diff, or roll back supergraph snapshots (`WP-M4-09`).
    Snapshot {
        /// `snapshot` subcommand.
        #[command(subcommand)]
        action: SnapshotSubcommand,
    },
    /// Validate or hot-reload Cedar authorization policies (`WP-M6-05`).
    Policy {
        /// `policy` subcommand.
        #[command(subcommand)]
        action: PolicySubcommand,
    },
    /// Verify or export hash-chained audit logs (`WP-M6-11`).
    Audit {
        /// `audit` subcommand.
        #[command(subcommand)]
        action: AuditSubcommand,
    },
    /// Fit offline temperature scaling and conformal calibration (`WP-M5-04`).
    Calibrate {
        /// Forwarded arguments for `WP-M5-04`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run reproducible protocol and routing benchmarks (`WP-M7-05`).
    Bench {
        /// Forwarded arguments for `WP-M7-05`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run source ingestion pipelines (`WP-M4-01`).
    Ingest {
        /// `ingest` subcommand.
        #[command(subcommand)]
        action: IngestSubcommand,
    },
    /// Inspect or cancel asynchronous agent tasks (`WP-M6-02`).
    Tasks {
        /// `tasks` subcommand.
        #[command(subcommand)]
        action: TasksSubcommand,
    },
    /// Purge expired state-store and blob records (`WP-M7-01`).
    Purge {
        /// Forwarded arguments for `WP-M7-01`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// `gap config {schema,check}` subcommands (`OPS-044`).
#[derive(Debug, Subcommand)]
pub enum ConfigSubcommand {
    /// Print the JSON Schema for `gap.toml` (`docs/config.schema.json`).
    Schema,
    /// Parse and validate `FILE` (including referenced OpenAPI specs) without starting the server.
    Check {
        /// Path to `gap.toml`.
        file: PathBuf,
    },
}

/// `gap models {pull,verify}` subcommands (`WP-M2-04`).
#[derive(Debug, Subcommand)]
pub enum ModelsSubcommand {
    /// Download and hash-verify declared model artifacts.
    Pull {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Verify local model artifact digests against `models/manifest.toml`.
    Verify {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// `gap proposals {list,show,approve,deny}` subcommands (`WP-M4-07`).
#[derive(Debug, Subcommand)]
pub enum ProposalsSubcommand {
    /// List ingestion proposals.
    List {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show details of a proposal.
    Show {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Approve a pending proposal.
    Approve {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Deny a pending proposal.
    Deny {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// `gap snapshot {show,diff,rollback}` subcommands (`WP-M4-09`).
#[derive(Debug, Subcommand)]
pub enum SnapshotSubcommand {
    /// Show active or historical snapshot metadata.
    Show {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Diff two snapshot generations.
    Diff {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Roll back to a previous snapshot generation.
    Rollback {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// `gap policy {check,reload}` subcommands (`WP-M6-05`).
#[derive(Debug, Subcommand)]
pub enum PolicySubcommand {
    /// Validate Cedar policy syntax and schema conformance.
    Check {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Trigger atomic Cedar policy reload.
    Reload {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// `gap audit {verify,export}` subcommands (`WP-M6-11`).
#[derive(Debug, Subcommand)]
pub enum AuditSubcommand {
    /// Verify audit hash-chain integrity.
    Verify {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Export audit records (`prov-jsonld`).
    Export {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// `gap ingest {run,once}` subcommands (`WP-M4-01`).
#[derive(Debug, Subcommand)]
pub enum IngestSubcommand {
    /// Run continuous ingestion worker loop.
    Run {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a single ingestion pass and exit.
    Once {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// `gap tasks {list,show,cancel}` subcommands (`WP-M6-02`).
#[derive(Debug, Subcommand)]
pub enum TasksSubcommand {
    /// List tasks.
    List {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Show task state and metadata.
    Show {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Request cancellation of an active task.
    Cancel {
        /// Forwarded arguments.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn stub_not_available(wp_id: &str, stderr: &mut dyn Write) -> i32 {
    let _ = writeln!(stderr, "not available in this build ({wp_id})");
    EXIT_CONFIG_ERROR
}

fn run_config_schema(stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    match GapConfig::json_schema_pretty() {
        Ok(schema) => {
            let _ = write!(stdout, "{schema}");
            EXIT_OK
        }
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            EXIT_CONFIG_ERROR
        }
    }
}

fn run_config_check(file: &Path, stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    match ServerFile::load(file) {
        Ok(sf) => {
            let profile = match sf.config().profile {
                Profile::Dev => "dev",
                Profile::Prod => "prod",
            };
            let _ = writeln!(
                stdout,
                "ok: {} (profile={profile}, branches={}, api_keys={})",
                file.display(),
                sf.config().branches.len(),
                sf.config().api_keys.len()
            );
            EXIT_OK
        }
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            EXIT_CONFIG_ERROR
        }
    }
}

/// Executes the `gap` CLI for `args` (including `argv[0]`), writing output to `stdout` and `stderr`
/// and returning the process exit code (`0`, `1`, or `2`).
pub async fn run_with_io<I, T>(args: I, stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(parsed) => parsed,
        Err(err) => match err.kind() {
            ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                let _ = write!(stdout, "{err}");
                return EXIT_OK;
            }
            _ => {
                let _ = write!(stderr, "{err}");
                return EXIT_CONFIG_ERROR;
            }
        },
    };

    match cli.command {
        Command::Serve { config, stdio } => {
            if let Err(err) = ServerFile::load(&config) {
                let _ = writeln!(stderr, "error: {err}");
                return EXIT_CONFIG_ERROR;
            }
            run_server_from_path(&config, stdio).await
        }
        Command::Init { dir, force } => run_init(&dir, force, stdout, stderr),
        Command::Doctor { config } => run_doctor(&config, stdout, stderr),
        Command::Config { action } => match action {
            ConfigSubcommand::Schema => run_config_schema(stdout, stderr),
            ConfigSubcommand::Check { file } => run_config_check(&file, stdout, stderr),
        },
        Command::Models { action } => match action {
            ModelsSubcommand::Pull { .. } | ModelsSubcommand::Verify { .. } => {
                stub_not_available("WP-M2-04", stderr)
            }
        },
        Command::Ingest { action } => match action {
            IngestSubcommand::Run { .. } | IngestSubcommand::Once { .. } => {
                stub_not_available("WP-M4-01", stderr)
            }
        },
        Command::Proposals { action } => match action {
            ProposalsSubcommand::List { .. }
            | ProposalsSubcommand::Show { .. }
            | ProposalsSubcommand::Approve { .. }
            | ProposalsSubcommand::Deny { .. } => stub_not_available("WP-M4-07", stderr),
        },
        Command::Snapshot { action } => match action {
            SnapshotSubcommand::Show { .. }
            | SnapshotSubcommand::Diff { .. }
            | SnapshotSubcommand::Rollback { .. } => stub_not_available("WP-M4-09", stderr),
        },
        Command::Calibrate { .. } => stub_not_available("WP-M5-04", stderr),
        Command::Tasks { action } => match action {
            TasksSubcommand::List { .. }
            | TasksSubcommand::Show { .. }
            | TasksSubcommand::Cancel { .. } => stub_not_available("WP-M6-02", stderr),
        },
        Command::Policy { action } => match action {
            PolicySubcommand::Check { .. } | PolicySubcommand::Reload { .. } => {
                stub_not_available("WP-M6-05", stderr)
            }
        },
        Command::Audit { action } => match action {
            AuditSubcommand::Verify { .. } | AuditSubcommand::Export { .. } => {
                stub_not_available("WP-M6-11", stderr)
            }
        },
        Command::Purge { .. } => stub_not_available("WP-M7-01", stderr),
        Command::Bench { .. } => stub_not_available("WP-M7-05", stderr),
    }
}

/// Runs the `gap` CLI from `std::env::args_os()` over standard streams.
pub async fn run_from_env() -> i32 {
    let mut out = io::stdout();
    let mut err = io::stderr();
    run_with_io(std::env::args_os(), &mut out, &mut err).await
}
