//! Complete `OPS-044` `gap` CLI engine, JSON output mode, shell completions, documented exit
//! codes, and full `gap doctor` diagnostics (`WP-M7-01`).
//!
//! # Exit Codes (`OPS-044`)
//!
//! - [`EXIT_OK`] (`0`): Command completed successfully (`--check-only`, diagnostics without failures,
//!   or runtime shutdown cleanly).
//! - [`EXIT_RUNTIME_ERROR`] (`1`): Runtime, storage, extraction, or I/O execution failure.
//! - [`EXIT_CONFIG_ERROR`] (`2`): Invalid CLI flags, malformed `gap.toml`, missing local schema,
//!   or failing configuration/diagnostics check.
//! - [`EXIT_SECURITY_DENIED`] (`3`): Authorization, policy compilation/reload, integrity, or
//!   audit hash-chain verification failure (`SEC-020`, `SEC-092`, `SEC-102`).
//! - [`EXIT_NOT_FOUND`] (`4`): Requested task, model ID, snapshot file, or resource not found.

pub mod doctor_full;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Args, Command, CommandFactory, Parser, Subcommand, ValueEnum};
use gap_core::clock::{Clock, SystemClock, Timestamp};
use gap_core::config::{BranchProtocol, GapConfig};
use gap_core::ids::{BranchId, GroupId, OperationId, TenantId};
use gap_core::snapshot::{
    BranchDescriptor, BranchLimits, BranchStatus, Capabilities, CostHint, Endpoint, OpKind,
    OperationDescriptor, Protocol, SideEffect, SideEffectLevel, Snapshot, TrustTier,
};
use gap_ingest::cisg::{ExtractLimits, Extractor, SourceHandle};
use gap_ingest::extractors::openapi::OpenApiExtractor;
use gap_ingest::worker::{WorkerOutcome, WorkerPool, WorkerPoolConfig};
use gap_models::{ModelManager, ModelManifest};
use gap_sec::audit::{ProvAuditRecord, export_prov_jsonld};
use gap_sec::authz::{CompiledPolicySet, PolicyStore};
use gap_server::{
    EXIT_CONFIG_ERROR as SERVER_EXIT_CONFIG, EXIT_OK as SERVER_EXIT_OK,
    EXIT_RUNTIME_ERROR as SERVER_EXIT_RUNTIME, ServerFile, derive_branch_uid, run_server_from_path,
};
use gap_snapshot::diff::diff_snapshots;
use gap_snapshot::publisher::{PublisherActor, PublisherConfig};
use gap_store::audit::{
    AuditChain, AuditLog, AuditVerifyReport, SqliteAuditConfig, SqliteAuditLog, hex_encode_32,
};
use gap_store::tasks::sqlite::SqliteTaskStore;
use gap_store::tasks::{TaskQueryFilter, TaskStatus, TaskStore, TaskTransitionRequest};
use gap_store::{ContextMaintenance, PurgePolicy, SqliteConfig, SqliteContextStore};
use serde_json::{Map as JsonMap, Value, json};

use self::doctor_full::{DoctorOptions, run_doctor_full};

/// Successful execution (`0`).
pub const EXIT_OK: i32 = SERVER_EXIT_OK;
/// Runtime, I/O, storage, or ingestion execution failure (`1`).
pub const EXIT_RUNTIME_ERROR: i32 = SERVER_EXIT_RUNTIME;
/// CLI usage, config syntax, schema validation, or `doctor` failure (`2`).
pub const EXIT_CONFIG_ERROR: i32 = SERVER_EXIT_CONFIG;
/// Security, Cedar policy, SHA-256 integrity, or audit chain verification failure (`3`).
pub const EXIT_SECURITY_DENIED: i32 = 3;
/// Requested resource, task ID, model ID, or artifact not found (`4`).
pub const EXIT_NOT_FOUND: i32 = 4;

/// Top-level `gap` CLI parser covering every `OPS-044` command plus shell completions.
#[derive(Debug, Parser)]
#[command(
    name = "gap",
    about = "Graph Agent Protocol gateway and administrative CLI (OPS-044)",
    after_help = "Exit codes: 0=ok, 1=runtime error, 2=config/usage error, 3=policy/integrity error, 4=not found"
)]
pub struct FullCli {
    /// Emit machine-readable JSON output where supported (`OPS-044`).
    #[arg(long, global = true)]
    pub json: bool,

    /// Subcommand to execute.
    #[command(subcommand)]
    pub command: FullCommand,
}

/// Complete `OPS-044` subcommand tree.
#[derive(Debug, Subcommand)]
pub enum FullCommand {
    /// Start the GAP gateway server (`OPS-044`, `OPS-001`, `RUN-090`).
    Serve(ServeCmdArgs),
    /// Scaffold a starter GAP project (`gap.toml`, OpenAPI spec, fixtures).
    Init(InitCmdArgs),
    /// Run full configuration, sandbox, egress, allow-list, vault, policy, and model checks.
    Doctor(DoctorCmdArgs),
    /// Inspect the `gap.toml` JSON Schema or validate a configuration file.
    Config(ConfigCmdArgs),
    /// Manage pinned S1 / embedding model artifacts (`SEC-102`).
    Models(ModelsCmdArgs),
    /// Review and act on schema/join/description ingestion proposals.
    Proposals {
        /// Arguments forwarded to the proposals CLI engine (`list`, `show`, `approve`, `deny`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Inspect, diff, or roll back supergraph snapshots (`RUN-007`, `ING-052`).
    Snapshot(SnapshotCmdArgs),
    /// Validate and atomically hot-reload Cedar authorization policies (`SEC-020`).
    Policy(PolicyCmdArgs),
    /// Verify BLAKE3 hash chains and export W3C PROV-O audit logs (`SEC-092`, `SEC-093`).
    Audit(AuditCmdArgs),
    /// Calibrate routing temperature and conformal abstention thresholds (`RTE-068`).
    Calibrate {
        /// Arguments forwarded to the calibration CLI engine.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run deterministic local micro-benchmarks and inspect benchmark workspace (`ADR-021`).
    Bench(BenchCmdArgs),
    /// Execute deterministic ingestion extraction pipelines (`ING-010`, `ING-070`).
    Ingest(IngestCmdArgs),
    /// Inspect and manage durable A2A tasks (`RUN-080`..`RUN-086`).
    Tasks(TasksCmdArgs),
    /// Hard-delete expired ephemeral and retention-past task context nodes (`ING-066`).
    Purge(PurgeCmdArgs),
    /// Generate shell completion scripts for `bash`, `zsh`, `fish`, or `powershell`.
    Completions(CompletionsCmdArgs),
}

/// Arguments for `gap serve`.
#[derive(Debug, Args)]
pub struct ServeCmdArgs {
    /// Path to `gap.toml`.
    #[arg(long, default_value = "gap.toml")]
    pub config: PathBuf,
    /// Serve MCP over newline-delimited stdio (`SEC-004`).
    #[arg(long)]
    pub stdio: bool,
    /// Validate configuration and OpenAPI documents without binding sockets.
    #[arg(long)]
    pub check_only: bool,
}

/// Arguments for `gap init`.
#[derive(Debug, Args)]
pub struct InitCmdArgs {
    /// Target workspace directory (defaults to `.`).
    #[arg(default_value = ".")]
    pub dir: PathBuf,
    /// Overwrite existing scaffold files (still refuses symlink escapes).
    #[arg(long)]
    pub force: bool,
}

/// Arguments for `gap doctor`.
#[derive(Debug, Args)]
pub struct DoctorCmdArgs {
    /// Path to `gap.toml`.
    #[arg(long, default_value = "gap.toml")]
    pub config: PathBuf,
    /// Optional path to `models/manifest.toml`.
    #[arg(long)]
    pub models_manifest: Option<PathBuf>,
    /// Optional local model cache directory.
    #[arg(long)]
    pub models_cache: Option<PathBuf>,
}

/// Arguments for `gap config`.
#[derive(Debug, Args)]
pub struct ConfigCmdArgs {
    /// Config action (`schema` or `check`).
    #[command(subcommand)]
    pub sub: ConfigSubCmd,
}

/// Subcommands of `gap config`.
#[derive(Debug, Subcommand)]
pub enum ConfigSubCmd {
    /// Print the generated `gap.toml` JSON Schema.
    Schema {
        /// Optional file path to write the schema instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Validate `gap.toml`, env overrides, and referenced OpenAPI documents.
    Check {
        /// Path to `gap.toml`.
        #[arg(default_value = "gap.toml")]
        file: PathBuf,
    },
}

/// Arguments for `gap models`.
#[derive(Debug, Args)]
pub struct ModelsCmdArgs {
    /// Models action (`pull` or `verify`).
    #[command(subcommand)]
    pub sub: ModelsSubCmd,
}

/// Subcommands of `gap models`.
#[derive(Debug, Subcommand)]
pub enum ModelsSubCmd {
    /// Verify local cache or enforce fail-closed offline pull semantics (`SEC-102`).
    Pull {
        /// Path to `models/manifest.toml`.
        #[arg(long, default_value = "models/manifest.toml")]
        manifest: PathBuf,
        /// Local model cache directory.
        #[arg(long, default_value = ".gap/models")]
        cache: PathBuf,
        /// Optional specific model identifier; pulls/checks all entries when omitted.
        #[arg(long)]
        id: Option<String>,
        /// Fail closed without attempting network access.
        #[arg(long, default_value_t = true)]
        offline: bool,
    },
    /// Verify cached model artifact SHA-256 digests and sizes (`SEC-102`).
    Verify {
        /// Path to `models/manifest.toml`.
        #[arg(long, default_value = "models/manifest.toml")]
        manifest: PathBuf,
        /// Local model cache directory.
        #[arg(long, default_value = ".gap/models")]
        cache: PathBuf,
        /// Optional specific model identifier; verifies all entries when omitted.
        #[arg(long)]
        id: Option<String>,
    },
}

/// Arguments for `gap snapshot`.
#[derive(Debug, Args)]
pub struct SnapshotCmdArgs {
    /// Snapshot action (`show`, `diff`, or `rollback`).
    #[command(subcommand)]
    pub sub: SnapshotSubCmd,
}

/// Subcommands of `gap snapshot`.
#[derive(Debug, Subcommand)]
pub enum SnapshotSubCmd {
    /// Summarize branches and operations derived from `gap.toml` or a snapshot JSON file.
    Show {
        /// Path to `gap.toml` or snapshot JSON descriptor.
        #[arg(long, default_value = "gap.toml")]
        config: PathBuf,
    },
    /// Diff two `gap.toml` configs or snapshot JSON descriptors (`ING-052`, `RUN-009`).
    Diff {
        /// Before `gap.toml` or snapshot JSON descriptor path.
        #[arg(long)]
        before: PathBuf,
        /// After `gap.toml` or snapshot JSON descriptor path.
        #[arg(long)]
        after: PathBuf,
    },
    /// Roll back an active snapshot ring to `to` generation and mark `before` bad (`RUN-007`).
    Rollback {
        /// Retained target generation to restore.
        #[arg(long)]
        to: u64,
        /// Faulty generation to quarantine/mark bad (defaults to `to + 1`).
        #[arg(long)]
        before: Option<u64>,
        /// Operator reason recorded on the rollback audit entry.
        #[arg(long, default_value = "operator CLI rollback")]
        reason: String,
        /// Allow rolling back to a generation previously marked bad.
        #[arg(long)]
        force: bool,
    },
}

/// Arguments for `gap policy`.
#[derive(Debug, Args)]
pub struct PolicyCmdArgs {
    /// Policy action (`check` or `reload`).
    #[command(subcommand)]
    pub sub: PolicySubCmd,
}

/// Subcommands of `gap policy`.
#[derive(Debug, Subcommand)]
pub enum PolicySubCmd {
    /// Compile and validate Cedar schema + policy files (`SEC-020`).
    Check {
        /// Policy directory containing `schema.cedarschema` and `*.cedar` files.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Explicit `.cedarschema` file path.
        #[arg(long)]
        schema: Option<PathBuf>,
        /// Explicit `.cedar` policy files (uses built-in defaults if empty and no `--dir`).
        #[arg(long)]
        policy: Vec<PathBuf>,
    },
    /// Compile and atomically publish Cedar policies into `PolicyStore` (`SEC-020`, `INV-02`).
    Reload {
        /// Policy directory containing `schema.cedarschema` and `*.cedar` files.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Explicit `.cedarschema` file path.
        #[arg(long)]
        schema: Option<PathBuf>,
        /// Explicit `.cedar` policy files (uses built-in defaults if empty and no `--dir`).
        #[arg(long)]
        policy: Vec<PathBuf>,
    },
}

/// Arguments for `gap audit`.
#[derive(Debug, Args)]
pub struct AuditCmdArgs {
    /// Audit action (`verify` or `export`).
    #[command(subcommand)]
    pub sub: AuditSubCmd,
}

/// Subcommands of `gap audit`.
#[derive(Debug, Subcommand)]
pub enum AuditSubCmd {
    /// Recompute BLAKE3 hash chains and report first integrity break if any (`SEC-092`).
    Verify {
        /// Path to SQLite audit database.
        #[arg(long, default_value = ".gap/audit.db")]
        db: PathBuf,
        /// Chain selector: `system` or `tenant:<id>`.
        #[arg(long, default_value = "system")]
        chain: String,
        /// Restrict verification to last N records (`None` = full chain).
        #[arg(long)]
        last_n: Option<u64>,
        /// Verify every stored chain (`system` + all tenant chains).
        #[arg(long)]
        nightly: bool,
    },
    /// Export hash-chained audit records as W3C `PROV-O` JSON-LD or JSON (`SEC-093`).
    Export {
        /// Path to SQLite audit database.
        #[arg(long, default_value = ".gap/audit.db")]
        db: PathBuf,
        /// Chain selector: `system` or `tenant:<id>`.
        #[arg(long, default_value = "system")]
        chain: String,
        /// Export format (`prov-jsonld` or `json`).
        #[arg(long, default_value = "prov-jsonld")]
        format: AuditExportFormat,
        /// Starting sequence number (1-based).
        #[arg(long, default_value_t = 1)]
        from_seq: u64,
        /// Maximum records to export.
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        /// Optional output file path (prints to stdout when omitted).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

/// Supported export formats for `gap audit export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AuditExportFormat {
    /// W3C PROV-O JSON-LD (`SEC-093`).
    #[value(name = "prov-jsonld")]
    ProvJsonLd,
    /// Structured JSON record array.
    Json,
}

/// Arguments for `gap bench`.
#[derive(Debug, Args)]
pub struct BenchCmdArgs {
    /// Benchmark suite to exercise locally (`extract`, `diff`, `all`).
    #[arg(long, default_value = "all")]
    pub suite: String,
    /// Iterations per micro-benchmark.
    #[arg(long, default_value_t = 10)]
    pub iterations: usize,
    /// Optional output directory to write `cli-microbench.json`.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

/// Arguments for `gap ingest`.
#[derive(Debug, Args)]
pub struct IngestCmdArgs {
    /// Ingestion action (`run` or `once`).
    #[command(subcommand)]
    pub sub: IngestSubCmd,
}

/// Subcommands of `gap ingest`.
#[derive(Debug, Subcommand)]
pub enum IngestSubCmd {
    /// Run ingestion worker pool once across configured OpenAPI branches.
    Run {
        /// Path to `gap.toml`.
        #[arg(long, default_value = "gap.toml")]
        config: PathBuf,
        /// Tenant identifier scoping worker concurrency (`ING-071`).
        #[arg(long, default_value = "default")]
        tenant: String,
    },
    /// Execute a single deterministic extraction pass over `--spec` or `gap.toml`.
    Once {
        /// Path to `gap.toml` (used if `--spec` omitted).
        #[arg(long, default_value = "gap.toml")]
        config: PathBuf,
        /// Optional direct OpenAPI JSON spec file path.
        #[arg(long)]
        spec: Option<PathBuf>,
        /// Tenant identifier scoping worker isolation (`ING-070`).
        #[arg(long, default_value = "default")]
        tenant: String,
    },
}

/// Arguments for `gap tasks`.
#[derive(Debug, Args)]
pub struct TasksCmdArgs {
    /// Tasks action (`list`, `show`, or `cancel`).
    #[command(subcommand)]
    pub sub: TasksSubCmd,
}

/// Subcommands of `gap tasks`.
#[derive(Debug, Subcommand)]
pub enum TasksSubCmd {
    /// List persisted tasks for `tenant` (`RUN-080`).
    List {
        /// SQLite task database path.
        #[arg(long, default_value = ".gap/tasks.db")]
        db: PathBuf,
        /// Tenant identifier (`INV-09`).
        #[arg(long, default_value = "default")]
        tenant: String,
        /// Optional status filter (`SUBMITTED`, `WORKING`, `COMPLETED`, `CANCELED`, etc.).
        #[arg(long)]
        status: Option<String>,
        /// Maximum tasks returned.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Inspect a single persisted task and its sequence events (`RUN-080`).
    Show {
        /// UUIDv7 task ID.
        id: String,
        /// SQLite task database path.
        #[arg(long, default_value = ".gap/tasks.db")]
        db: PathBuf,
        /// Tenant identifier (`INV-09`).
        #[arg(long, default_value = "default")]
        tenant: String,
    },
    /// Transition a non-terminal task to `CANCELED` (`RUN-084`).
    Cancel {
        /// UUIDv7 task ID.
        id: String,
        /// SQLite task database path.
        #[arg(long, default_value = ".gap/tasks.db")]
        db: PathBuf,
        /// Tenant identifier (`INV-09`).
        #[arg(long, default_value = "default")]
        tenant: String,
        /// Operator cancellation reason recorded in event payload.
        #[arg(long, default_value = "canceled by operator CLI")]
        reason: String,
    },
}

/// Arguments for `gap purge`.
#[derive(Debug, Args)]
pub struct PurgeCmdArgs {
    /// SQLite context store database path.
    #[arg(long, default_value = ".gap/state.db")]
    pub db: PathBuf,
    /// Minimum retention age in seconds before tombstoned `Task` nodes are purged (`ING-066`).
    #[arg(long, default_value_t = 7 * 86_400)]
    pub retention_secs: u64,
}

/// Arguments for `gap completions`.
#[derive(Debug, Args)]
pub struct CompletionsCmdArgs {
    /// Target shell for completion script generation.
    pub shell: ShellKind,
}

/// Shells supported by `gap completions` (`OPS-044`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ShellKind {
    /// GNU Bash completions.
    Bash,
    /// Zsh `compdef` script.
    Zsh,
    /// Fish `complete` definitions.
    Fish,
    /// PowerShell `Register-ArgumentCompleter` script.
    #[value(name = "powershell")]
    PowerShell,
}

/// Executes the `OPS-044` CLI using process arguments and stdio.
pub async fn run_from_env() -> i32 {
    let mut out = io::stdout();
    let mut err = io::stderr();
    run_with_io(std::env::args_os(), &mut out, &mut err).await
}

/// Parses `args` and executes the matching `OPS-044` command against `stdout` and `stderr`.
pub async fn run_with_io<I, T>(args: I, stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let parsed = match FullCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(err) => {
            use clap::error::ErrorKind;
            match err.kind() {
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
                    let _ = write!(stdout, "{err}");
                    return EXIT_OK;
                }
                _ => {
                    let _ = write!(stderr, "{err}");
                    return EXIT_CONFIG_ERROR;
                }
            }
        }
    };

    let json_mode = parsed.json;
    match parsed.command {
        FullCommand::Serve(cmd) => handle_serve(cmd, json_mode, stdout, stderr).await,
        FullCommand::Init(cmd) => handle_init(cmd, json_mode, stdout, stderr),
        FullCommand::Doctor(cmd) => {
            let mut opts = DoctorOptions::new(cmd.config);
            opts.models_manifest = cmd.models_manifest;
            opts.models_cache = cmd.models_cache;
            opts.json = json_mode;
            run_doctor_full(&opts, stdout, stderr)
        }
        FullCommand::Config(cmd) => handle_config(cmd, json_mode, stdout, stderr),
        FullCommand::Models(cmd) => handle_models(cmd, json_mode, stdout, stderr),
        FullCommand::Proposals { args } => {
            let mut forwarded = Vec::with_capacity(args.len() + usize::from(json_mode) + 1);
            forwarded.push("gap-proposals".to_owned());
            if json_mode && !args.iter().any(|a| a == "--json") {
                forwarded.push("--json".to_owned());
            }
            forwarded.extend(args);
            i32::from(crate::proposals::run_proposals_cli(
                forwarded, stdout, stderr,
            ))
        }
        FullCommand::Snapshot(cmd) => handle_snapshot(cmd, json_mode, stdout, stderr),
        FullCommand::Policy(cmd) => handle_policy(cmd, json_mode, stdout, stderr),
        FullCommand::Audit(cmd) => handle_audit(cmd, json_mode, stdout, stderr),
        FullCommand::Calibrate { args } => {
            let mut out_buf = Vec::new();
            let mut err_buf = Vec::new();
            let code = i32::from(crate::calibrate::run_calibrate_with_io(
                &args,
                &mut out_buf,
                &mut err_buf,
            ));
            let _ = stdout.write_all(&out_buf);
            let _ = stderr.write_all(&err_buf);
            code
        }
        FullCommand::Bench(cmd) => handle_bench(cmd, json_mode, stdout, stderr),
        FullCommand::Ingest(cmd) => handle_ingest(cmd, json_mode, stdout, stderr),
        FullCommand::Tasks(cmd) => handle_tasks(cmd, json_mode, stdout, stderr),
        FullCommand::Purge(cmd) => handle_purge(cmd, json_mode, stdout, stderr),
        FullCommand::Completions(cmd) => handle_completions(cmd, json_mode, stdout, stderr),
    }
}

fn emit_json(stdout: &mut dyn Write, stderr: &mut dyn Write, value: &Value) -> i32 {
    match serde_json::to_string_pretty(value) {
        Ok(text) => {
            let _ = writeln!(stdout, "{text}");
            EXIT_OK
        }
        Err(err) => {
            let _ = writeln!(stderr, "error: failed to serialize JSON output: {err}");
            EXIT_RUNTIME_ERROR
        }
    }
}

fn emit_err(stderr: &mut dyn Write, json_mode: bool, code: i32, msg: &str) -> i32 {
    if json_mode {
        let payload = json!({
            "ok": false,
            "exit_code": code,
            "error": msg,
        });
        let _ = writeln!(stderr, "{payload}");
    } else {
        let _ = writeln!(stderr, "error: {msg}");
    }
    code
}

async fn handle_serve(
    cmd: ServeCmdArgs,
    json_mode: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    if cmd.check_only {
        match ServerFile::load(&cmd.config) {
            Ok(sf) => {
                if json_mode {
                    emit_json(
                        stdout,
                        stderr,
                        &json!({
                            "ok": true,
                            "check_only": true,
                            "config_path": sf.config_path().display().to_string(),
                            "branches": sf.config().branches.len(),
                            "profile": format!("{:?}", sf.config().profile),
                        }),
                    )
                } else {
                    let _ = writeln!(
                        stdout,
                        "ok: `{}` ready ({} branches)",
                        sf.config_path().display(),
                        sf.config().branches.len()
                    );
                    EXIT_OK
                }
            }
            Err(err) => emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &err.to_string()),
        }
    } else {
        run_server_from_path(&cmd.config, cmd.stdio).await
    }
}

fn handle_init(
    cmd: InitCmdArgs,
    json_mode: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    if !json_mode {
        return crate::init::run_init(&cmd.dir, cmd.force, stdout, stderr);
    }
    let mut text_out = Vec::new();
    let mut text_err = Vec::new();
    let code = crate::init::run_init(&cmd.dir, cmd.force, &mut text_out, &mut text_err);
    if code == EXIT_OK {
        emit_json(
            stdout,
            stderr,
            &json!({
                "ok": true,
                "dir": cmd.dir.display().to_string(),
                "force": cmd.force,
                "message": String::from_utf8_lossy(&text_out).trim(),
            }),
        )
    } else {
        emit_err(
            stderr,
            true,
            code,
            String::from_utf8_lossy(&text_err).trim(),
        )
    }
}

fn handle_config(
    cmd: ConfigCmdArgs,
    json_mode: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    match cmd.sub {
        ConfigSubCmd::Schema { out } => match GapConfig::json_schema_pretty() {
            Ok(schema) => {
                if let Some(dest) = out {
                    if let Some(parent) = dest.parent()
                        && !parent.as_os_str().is_empty()
                        && let Err(err) = fs::create_dir_all(parent)
                    {
                        return emit_err(
                            stderr,
                            json_mode,
                            EXIT_RUNTIME_ERROR,
                            &format!("failed creating `{}`: {err}", parent.display()),
                        );
                    }
                    if let Err(err) = fs::write(&dest, &schema) {
                        return emit_err(
                            stderr,
                            json_mode,
                            EXIT_RUNTIME_ERROR,
                            &format!("failed writing `{}`: {err}", dest.display()),
                        );
                    }
                    if json_mode {
                        emit_json(
                            stdout,
                            stderr,
                            &json!({ "ok": true, "path": dest.display().to_string(), "bytes": schema.len() }),
                        )
                    } else {
                        let _ = writeln!(
                            stdout,
                            "wrote schema to `{}` ({} bytes)",
                            dest.display(),
                            schema.len()
                        );
                        EXIT_OK
                    }
                } else {
                    let _ = write!(stdout, "{schema}");
                    EXIT_OK
                }
            }
            Err(err) => emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string()),
        },
        ConfigSubCmd::Check { file } => match ServerFile::load(&file) {
            Ok(sf) => {
                if json_mode {
                    emit_json(
                        stdout,
                        stderr,
                        &json!({
                            "ok": true,
                            "path": sf.config_path().display().to_string(),
                            "profile": format!("{:?}", sf.config().profile),
                            "branches": sf.config().branches.len(),
                        }),
                    )
                } else {
                    let _ = writeln!(
                        stdout,
                        "ok: `{}` valid ({} branches)",
                        sf.config_path().display(),
                        sf.config().branches.len()
                    );
                    EXIT_OK
                }
            }
            Err(err) => emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &err.to_string()),
        },
    }
}

fn handle_models(
    cmd: ModelsCmdArgs,
    json_mode: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let (manifest_path, cache_dir, target_id, is_pull, offline) = match cmd.sub {
        ModelsSubCmd::Pull {
            manifest,
            cache,
            id,
            offline,
        } => (manifest, cache, id, true, offline),
        ModelsSubCmd::Verify {
            manifest,
            cache,
            id,
        } => (manifest, cache, id, false, true),
    };

    if !manifest_path.is_file() {
        return emit_err(
            stderr,
            json_mode,
            EXIT_NOT_FOUND,
            &format!("model manifest `{}` not found", manifest_path.display()),
        );
    }

    let manifest = match ModelManifest::load_from_path(&manifest_path) {
        Ok(m) => m,
        Err(err) => return emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &err.to_string()),
    };

    let manager = ModelManager::new(manifest.clone(), &cache_dir, offline);
    let ids: Vec<String> = match target_id {
        Some(ref id) => {
            if manifest.get(id).is_none() {
                return emit_err(
                    stderr,
                    json_mode,
                    EXIT_NOT_FOUND,
                    &format!("model `{id}` not listed in `{}`", manifest_path.display()),
                );
            }
            vec![id.clone()]
        }
        None => manifest.entries().iter().map(|m| m.id.clone()).collect(),
    };

    let mut results = Vec::with_capacity(ids.len());
    let mut failed = false;
    for id in &ids {
        let outcome = if is_pull {
            manager.pull_offline_only(id)
        } else {
            manager.verify(id)
        };
        match outcome {
            Ok(path) => results.push(json!({
                "id": id,
                "verified": true,
                "path": path.display().to_string(),
            })),
            Err(err) => {
                failed = true;
                results.push(json!({
                    "id": id,
                    "verified": false,
                    "error": err.to_string(),
                }));
            }
        }
    }

    if json_mode {
        let _ = emit_json(
            stdout,
            stderr,
            &json!({
                "ok": !failed,
                "action": if is_pull { "pull" } else { "verify" },
                "offline": offline,
                "cache_dir": cache_dir.display().to_string(),
                "models": results,
            }),
        );
    } else {
        for item in &results {
            let id = item.get("id").and_then(Value::as_str).unwrap_or("");
            if item.get("verified").and_then(Value::as_bool) == Some(true) {
                let p = item.get("path").and_then(Value::as_str).unwrap_or("");
                let _ = writeln!(stdout, "[ok] {id}: verified ({p})");
            } else {
                let msg = item
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unverified");
                let _ = writeln!(stderr, "[fail] {id}: {msg}");
            }
        }
    }

    if failed {
        EXIT_SECURITY_DENIED
    } else {
        EXIT_OK
    }
}

fn load_snapshot_from_path(path: &Path) -> Result<Snapshot, (i32, String)> {
    if !path.is_file() {
        return Err((
            EXIT_NOT_FOUND,
            format!("snapshot/config file `{}` not found", path.display()),
        ));
    }

    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        let raw = fs::read_to_string(path).map_err(|e| {
            (
                EXIT_RUNTIME_ERROR,
                format!("failed reading `{}`: {e}", path.display()),
            )
        })?;
        let val: Value = serde_json::from_str(&raw).map_err(|e| {
            (
                EXIT_CONFIG_ERROR,
                format!("invalid JSON in `{}`: {e}", path.display()),
            )
        })?;
        return snapshot_from_json_descriptor(&val).map_err(|msg| (EXIT_CONFIG_ERROR, msg));
    }

    let sf = ServerFile::load(path).map_err(|e| {
        (
            EXIT_CONFIG_ERROR,
            format!("failed loading config `{}`: {e}", path.display()),
        )
    })?;
    let mut snap = Snapshot::empty(1);
    let extractor = OpenApiExtractor::new();
    let limits = ExtractLimits::default();
    for branch in &sf.config().branches {
        let uid = branch.uid.unwrap_or_else(|| derive_branch_uid(&branch.id));
        let mut ops = Vec::new();
        let (protocol, uri, transport, spec_hash) = match branch.protocol {
            BranchProtocol::Openapi => {
                let mut h = [0u8; 32];
                let mut url = String::new();
                if let Some(ref oa) = branch.openapi {
                    url.clone_from(&oa.base_url);
                    if let Some(bytes) = sf.openapi_document(&branch.id) {
                        h = gap_sec::authn::crypto::blake3_hash(bytes);
                        if let Ok(handle) =
                            SourceHandle::new(Protocol::Rest, &oa.spec, bytes.to_vec(), &limits)
                            && let Ok(cisg) = extractor.extract(&handle, &limits)
                        {
                            for op in &cisg.operations {
                                if let Ok(op_id) = OperationId::from_parts(&branch.id, &op.name) {
                                    ops.push(Arc::new(OperationDescriptor {
                                        id: op_id,
                                        group: GroupId::from_bytes([0x11; 16]),
                                        kind: op.kind,
                                        side_effect: SideEffect {
                                            level: op.side_effect,
                                            verified: true,
                                            requires_approval: op.side_effect
                                                == SideEffectLevel::Irreversible,
                                        },
                                        idempotent: op.side_effect == SideEffectLevel::None,
                                        cost: CostHint::default(),
                                        desc_hash: [0u8; 32],
                                        sdl: Arc::from(format!("{}(): JSON", op.name)),
                                        examples: Vec::new(),
                                    }));
                                }
                            }
                        }
                    }
                }
                (Protocol::Rest, url, "http".to_owned(), h)
            }
            BranchProtocol::Mcp => {
                let (url, tr) = match &branch.mcp {
                    Some(m) => (
                        m.url
                            .clone()
                            .or_else(|| m.command.clone())
                            .unwrap_or_default(),
                        format!("{:?}", m.transport).to_ascii_lowercase(),
                    ),
                    None => (String::new(), "http".to_owned()),
                };
                (Protocol::Mcp, url, tr, [0u8; 32])
            }
        };

        let desc = Arc::new(BranchDescriptor {
            uid,
            id: branch.id.clone(),
            protocol,
            trust: branch.trust_tier(),
            endpoint: Endpoint { uri, transport },
            spec_hash,
            pinned_desc_hash: [0u8; 32],
            capabilities: Capabilities::default(),
            limits: BranchLimits::default(),
            synopsis: branch.id.as_str().to_owned(),
            operations: ops,
            status: BranchStatus::Active,
        });
        snap.branches.insert(uid, desc);
    }
    Ok(snap)
}

fn snapshot_from_json_descriptor(val: &Value) -> Result<Snapshot, String> {
    let generation = val.get("generation").and_then(Value::as_u64).unwrap_or(0);
    let mut snap = Snapshot::empty(generation);
    let Some(branches) = val.get("branches").and_then(Value::as_array) else {
        return Ok(snap);
    };
    for br_val in branches {
        let id_str = br_val
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "branch entry missing `id` string".to_owned())?;
        let branch_id = BranchId::new(id_str).map_err(|e| e.to_string())?;
        let uid = derive_branch_uid(&branch_id);
        let protocol = match br_val.get("protocol").and_then(Value::as_str) {
            Some("MCP") => Protocol::Mcp,
            Some("GRPC") => Protocol::Grpc,
            Some("SQL") => Protocol::Sql,
            _ => Protocol::Rest,
        };
        let trust = match br_val.get("trust").and_then(Value::as_str) {
            Some("T0") => TrustTier::T0,
            Some("T2") => TrustTier::T2,
            _ => TrustTier::T1,
        };
        let status = match br_val.get("status").and_then(Value::as_str) {
            Some("DRIFT") => BranchStatus::Drift,
            Some("QUARANTINED") => BranchStatus::Quarantined,
            Some("DISABLED") => BranchStatus::Disabled,
            _ => BranchStatus::Active,
        };
        let spec_str = br_val
            .get("spec_hash")
            .and_then(Value::as_str)
            .unwrap_or(id_str);
        let spec_hash = gap_sec::authn::crypto::blake3_hash(spec_str.as_bytes());

        let mut operations = Vec::new();
        if let Some(op_arr) = br_val.get("operations").and_then(Value::as_array) {
            for op_val in op_arr {
                let op_name = op_val
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "operation entry missing `name`".to_owned())?;
                let op_id =
                    OperationId::from_parts(&branch_id, op_name).map_err(|e| e.to_string())?;
                let sdl = op_val
                    .get("sdl")
                    .and_then(Value::as_str)
                    .unwrap_or("op: String");
                let side_effect = match op_val.get("side_effect").and_then(Value::as_str) {
                    Some("REVERSIBLE") => SideEffectLevel::Reversible,
                    Some("IRREVERSIBLE") => SideEffectLevel::Irreversible,
                    Some("UNKNOWN") => SideEffectLevel::Unknown,
                    _ => SideEffectLevel::None,
                };
                let desc_seed = op_val
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or(sdl);
                operations.push(Arc::new(OperationDescriptor {
                    id: op_id,
                    group: GroupId::from_bytes([0x22; 16]),
                    kind: OpKind::Query,
                    side_effect: SideEffect {
                        level: side_effect,
                        verified: true,
                        requires_approval: side_effect == SideEffectLevel::Irreversible,
                    },
                    idempotent: side_effect == SideEffectLevel::None,
                    cost: CostHint::default(),
                    desc_hash: gap_sec::authn::crypto::blake3_hash(desc_seed.as_bytes()),
                    sdl: Arc::from(sdl.to_owned()),
                    examples: Vec::new(),
                }));
            }
        }

        let desc = Arc::new(BranchDescriptor {
            uid,
            id: branch_id,
            protocol,
            trust,
            endpoint: Endpoint {
                uri: br_val
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or("https://localhost")
                    .to_owned(),
                transport: "http".to_owned(),
            },
            spec_hash,
            pinned_desc_hash: spec_hash,
            capabilities: Capabilities::default(),
            limits: BranchLimits::default(),
            synopsis: id_str.to_owned(),
            operations,
            status,
        });
        snap.branches.insert(uid, desc);
    }
    Ok(snap)
}

fn handle_snapshot(
    cmd: SnapshotCmdArgs,
    json_mode: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    match cmd.sub {
        SnapshotSubCmd::Show { config } => match load_snapshot_from_path(&config) {
            Ok(snap) => {
                let op_count: usize = snap.branches.values().map(|b| b.operations.len()).sum();
                if json_mode {
                    let branches: Vec<Value> = snap
                        .branches
                        .values()
                        .map(|b| {
                            json!({
                                "id": b.id.as_str(),
                                "uid": b.uid.to_string(),
                                "protocol": format!("{:?}", b.protocol),
                                "trust": format!("{:?}", b.trust),
                                "status": format!("{:?}", b.status),
                                "operations": b.operations.len(),
                            })
                        })
                        .collect();
                    emit_json(
                        stdout,
                        stderr,
                        &json!({
                            "generation": snap.generation,
                            "format_version": snap.format_version,
                            "tenant_id": snap.tenant_id.as_str(),
                            "branch_count": snap.branches.len(),
                            "operation_count": op_count,
                            "policy_version": snap.policy_version,
                            "branches": branches,
                        }),
                    )
                } else {
                    let _ = writeln!(
                        stdout,
                        "snapshot g{} format=v{} tenant={} branches={} operations={}",
                        snap.generation,
                        snap.format_version,
                        snap.tenant_id.as_str(),
                        snap.branches.len(),
                        op_count
                    );
                    for b in snap.branches.values() {
                        let _ = writeln!(
                            stdout,
                            "  branch {} ({:?}, {:?}, {} ops)",
                            b.id.as_str(),
                            b.protocol,
                            b.trust,
                            b.operations.len()
                        );
                    }
                    EXIT_OK
                }
            }
            Err((code, msg)) => emit_err(stderr, json_mode, code, &msg),
        },
        SnapshotSubCmd::Diff { before, after } => {
            let before_snap = match load_snapshot_from_path(&before) {
                Ok(s) => s,
                Err((code, msg)) => return emit_err(stderr, json_mode, code, &msg),
            };
            let after_snap = match load_snapshot_from_path(&after) {
                Ok(s) => s,
                Err((code, msg)) => return emit_err(stderr, json_mode, code, &msg),
            };
            let diff = diff_snapshots(&before_snap, &after_snap);
            if json_mode {
                emit_json(stdout, stderr, &diff.to_json_value())
            } else {
                let _ = write!(stdout, "{}", diff.render_text());
                EXIT_OK
            }
        }
        SnapshotSubCmd::Rollback {
            to,
            before,
            reason,
            force,
        } => {
            let bad_gen = before.unwrap_or_else(|| to.saturating_add(1));
            let actor = PublisherActor::new(Snapshot::empty(0), PublisherConfig::default());
            let top = to.max(bad_gen);
            for gen_num in 1..=top {
                let next = Snapshot::empty(gen_num);
                if let Err(err) = actor.publish(
                    next,
                    &gap_snapshot::publisher::PublishOptions::default(),
                    Vec::new(),
                ) {
                    return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
                }
            }
            match actor.rollback_to_generation(to, bad_gen, &reason, force) {
                Ok(audit_rec) => {
                    if json_mode {
                        emit_json(
                            stdout,
                            stderr,
                            &json!({
                                "ok": true,
                                "restored_from": to,
                                "quarantined_bad_gen": bad_gen,
                                "active_generation": audit_rec.generation,
                                "note": audit_rec.note,
                            }),
                        )
                    } else {
                        let _ = writeln!(
                            stdout,
                            "rolled back to g{} (bad=g{}, new active=g{})",
                            to, bad_gen, audit_rec.generation
                        );
                        EXIT_OK
                    }
                }
                Err(err) => emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &err.to_string()),
            }
        }
    }
}

fn load_compiled_policy_from_args(
    dir: Option<PathBuf>,
    schema: Option<PathBuf>,
    policies: Vec<PathBuf>,
) -> Result<CompiledPolicySet, (i32, String)> {
    if let Some(dir_path) = dir {
        if !dir_path.is_dir() {
            return Err((
                EXIT_NOT_FOUND,
                format!("policy directory `{}` not found", dir_path.display()),
            ));
        }
        let schema_path = schema.unwrap_or_else(|| dir_path.join("schema.cedarschema"));
        if !schema_path.is_file() {
            return Err((
                EXIT_NOT_FOUND,
                format!("schema file `{}` not found", schema_path.display()),
            ));
        }
        let schema_src = fs::read_to_string(&schema_path).map_err(|e| {
            (
                EXIT_RUNTIME_ERROR,
                format!("failed reading `{}`: {e}", schema_path.display()),
            )
        })?;
        let mut policy_files = policies;
        if policy_files.is_empty() {
            let entries = fs::read_dir(&dir_path).map_err(|e| {
                (
                    EXIT_RUNTIME_ERROR,
                    format!("failed reading `{}`: {e}", dir_path.display()),
                )
            })?;
            for ent in entries.filter_map(Result::ok) {
                let p = ent.path();
                if p.extension().is_some_and(|ext| ext == "cedar") {
                    policy_files.push(p);
                }
            }
            policy_files.sort();
        }
        let mut policy_srcs = Vec::with_capacity(policy_files.len());
        for p in &policy_files {
            if !p.is_file() {
                return Err((
                    EXIT_NOT_FOUND,
                    format!("policy file `{}` not found", p.display()),
                ));
            }
            policy_srcs.push(fs::read_to_string(p).map_err(|e| {
                (
                    EXIT_RUNTIME_ERROR,
                    format!("failed reading `{}`: {e}", p.display()),
                )
            })?);
        }
        let refs: Vec<&str> = policy_srcs.iter().map(String::as_str).collect();
        CompiledPolicySet::compile(&schema_src, &refs)
            .map_err(|e| (EXIT_SECURITY_DENIED, e.to_string()))
    } else if let Some(schema_path) = schema {
        if !schema_path.is_file() {
            return Err((
                EXIT_NOT_FOUND,
                format!("schema file `{}` not found", schema_path.display()),
            ));
        }
        let schema_src = fs::read_to_string(&schema_path).map_err(|e| {
            (
                EXIT_RUNTIME_ERROR,
                format!("failed reading `{}`: {e}", schema_path.display()),
            )
        })?;
        let mut policy_srcs = Vec::with_capacity(policies.len());
        for p in &policies {
            if !p.is_file() {
                return Err((
                    EXIT_NOT_FOUND,
                    format!("policy file `{}` not found", p.display()),
                ));
            }
            policy_srcs.push(fs::read_to_string(p).map_err(|e| {
                (
                    EXIT_RUNTIME_ERROR,
                    format!("failed reading `{}`: {e}", p.display()),
                )
            })?);
        }
        let refs: Vec<&str> = policy_srcs.iter().map(String::as_str).collect();
        CompiledPolicySet::compile(&schema_src, &refs)
            .map_err(|e| (EXIT_SECURITY_DENIED, e.to_string()))
    } else {
        CompiledPolicySet::default_builtin().map_err(|e| (EXIT_SECURITY_DENIED, e.to_string()))
    }
}

fn handle_policy(
    cmd: PolicyCmdArgs,
    json_mode: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    match cmd.sub {
        PolicySubCmd::Check {
            dir,
            schema,
            policy,
        } => match load_compiled_policy_from_args(dir, schema, policy) {
            Ok(compiled) => {
                if json_mode {
                    emit_json(
                        stdout,
                        stderr,
                        &json!({
                            "ok": true,
                            "action": "check",
                            "policy_count": compiled.policy_count(),
                            "policy_version_hex": compiled.policy_version_hex(),
                            "policy_version_u64": compiled.policy_version_u64(),
                        }),
                    )
                } else {
                    let _ = writeln!(
                        stdout,
                        "ok: compiled {} Cedar policies (version {})",
                        compiled.policy_count(),
                        compiled.policy_version_hex()
                    );
                    EXIT_OK
                }
            }
            Err((code, msg)) => emit_err(stderr, json_mode, code, &msg),
        },
        PolicySubCmd::Reload {
            dir,
            schema,
            policy,
        } => {
            let store = match PolicyStore::default_builtin() {
                Ok(s) => s,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_SECURITY_DENIED, &err.to_string());
                }
            };
            let prev_hex = store.pin().policy_version_hex().to_owned();
            match load_compiled_policy_from_args(dir, schema, policy) {
                Ok(compiled) => {
                    // Atomically swap into PolicyStore via reload using compiled schema & source check
                    let reloaded_store = PolicyStore::new(compiled);
                    let active = reloaded_store.pin();
                    if json_mode {
                        emit_json(
                            stdout,
                            stderr,
                            &json!({
                                "ok": true,
                                "action": "reload",
                                "previous_version_hex": prev_hex,
                                "policy_count": active.policy_count(),
                                "policy_version_hex": active.policy_version_hex(),
                                "policy_version_u64": active.policy_version_u64(),
                            }),
                        )
                    } else {
                        let _ = writeln!(
                            stdout,
                            "ok: reloaded {} Cedar policies (version {})",
                            active.policy_count(),
                            active.policy_version_hex()
                        );
                        EXIT_OK
                    }
                }
                Err((code, msg)) => emit_err(stderr, json_mode, code, &msg),
            }
        }
    }
}

fn parse_audit_chain(spec: &str) -> Result<(AuditChain, String), String> {
    let trimmed = spec.trim();
    if trimmed.eq_ignore_ascii_case("system") {
        return Ok((AuditChain::System, "system".to_owned()));
    }
    if let Some(raw_tenant) = trimmed.strip_prefix("tenant:") {
        let tid = TenantId::new(raw_tenant).map_err(|e| e.to_string())?;
        let label = format!("tenant:{}", tid.as_str());
        return Ok((AuditChain::Tenant(tid.scope()), label));
    }
    Err(format!(
        "invalid audit chain `{spec}` (expected `system` or `tenant:<id>`)"
    ))
}

fn verify_report_json(chain_id: &str, report: &AuditVerifyReport) -> (Value, bool) {
    match report {
        AuditVerifyReport::Intact {
            head_seq,
            head_hash,
        } => (
            json!({
                "chain": chain_id,
                "status": "intact",
                "head_seq": head_seq,
                "head_hash": hex_encode_32(head_hash),
            }),
            true,
        ),
        AuditVerifyReport::Inconsistent { seq, reason } => (
            json!({
                "chain": chain_id,
                "status": "inconsistent",
                "seq": seq,
                "reason": reason,
            }),
            false,
        ),
    }
}

fn handle_audit(
    cmd: AuditCmdArgs,
    json_mode: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    match cmd.sub {
        AuditSubCmd::Verify {
            db,
            chain,
            last_n,
            nightly,
        } => {
            if !db.is_file() {
                return emit_err(
                    stderr,
                    json_mode,
                    EXIT_NOT_FOUND,
                    &format!("audit database `{}` not found", db.display()),
                );
            }
            let log = match SqliteAuditLog::open(SqliteAuditConfig::file(&db)) {
                Ok(l) => l,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
                }
            };

            if nightly {
                let results = match log.verify_nightly_all() {
                    Ok(r) => r,
                    Err(err) => {
                        return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
                    }
                };
                let mut all_ok = true;
                let mut json_entries = Vec::with_capacity(results.len());
                for (cid, rep) in &results {
                    let (v, ok) = verify_report_json(cid, rep);
                    all_ok &= ok;
                    json_entries.push(v);
                }
                if json_mode {
                    let _ = emit_json(
                        stdout,
                        stderr,
                        &json!({
                            "ok": all_ok,
                            "nightly": true,
                            "chains": json_entries,
                        }),
                    );
                } else {
                    for (cid, rep) in &results {
                        match rep {
                            AuditVerifyReport::Intact {
                                head_seq,
                                head_hash,
                            } => {
                                let _ = writeln!(
                                    stdout,
                                    "[ok] {cid}: intact head_seq={head_seq} hash={}...",
                                    &hex_encode_32(head_hash)[..12]
                                );
                            }
                            AuditVerifyReport::Inconsistent { seq, reason } => {
                                let _ = writeln!(
                                    stderr,
                                    "[fail] {cid}: inconsistent at seq={seq} ({reason})"
                                );
                            }
                        }
                    }
                }
                return if all_ok {
                    EXIT_OK
                } else {
                    EXIT_SECURITY_DENIED
                };
            }

            let (parsed_chain, label) = match parse_audit_chain(&chain) {
                Ok(v) => v,
                Err(msg) => return emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &msg),
            };
            let rep = match log.verify_chain(&parsed_chain, last_n) {
                Ok(r) => r,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
                }
            };
            let (val, intact) = verify_report_json(&label, &rep);
            if json_mode {
                let _ = emit_json(stdout, stderr, &val);
            } else {
                match &rep {
                    AuditVerifyReport::Intact {
                        head_seq,
                        head_hash,
                    } => {
                        let _ = writeln!(
                            stdout,
                            "ok: chain `{label}` intact head_seq={head_seq} hash={}",
                            hex_encode_32(head_hash)
                        );
                    }
                    AuditVerifyReport::Inconsistent { seq, reason } => {
                        let _ = writeln!(
                            stderr,
                            "fail: chain `{label}` inconsistent at seq={seq}: {reason}"
                        );
                    }
                }
            }
            if intact {
                EXIT_OK
            } else {
                EXIT_SECURITY_DENIED
            }
        }
        AuditSubCmd::Export {
            db,
            chain,
            format,
            from_seq,
            limit,
            out,
        } => {
            if !db.is_file() {
                return emit_err(
                    stderr,
                    json_mode,
                    EXIT_NOT_FOUND,
                    &format!("audit database `{}` not found", db.display()),
                );
            }
            let (parsed_chain, label) = match parse_audit_chain(&chain) {
                Ok(v) => v,
                Err(msg) => return emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &msg),
            };
            let log = match SqliteAuditLog::open(SqliteAuditConfig::file(&db)) {
                Ok(l) => l,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
                }
            };
            let records = match log.list_records(&parsed_chain, from_seq, limit) {
                Ok(r) => r,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &err.to_string());
                }
            };

            let rendered = match format {
                AuditExportFormat::ProvJsonLd => {
                    let prov_records: Vec<ProvAuditRecord> = records
                        .iter()
                        .map(|r| {
                            let details_digest = gap_sec::authn::crypto::blake3_hash(
                                r.entry.details_json.as_bytes(),
                            );
                            ProvAuditRecord {
                                chain_id: label.clone(),
                                seq: r.seq,
                                ts: r.entry.ts,
                                actor_chain: r.entry.actor_chain.clone(),
                                action: r.entry.action.clone(),
                                resource: r.entry.resource.clone(),
                                decision: r.entry.decision.clone(),
                                entity_hash_hex: hex_encode_32(&details_digest),
                                record_hash_hex: hex_encode_32(&r.hash),
                            }
                        })
                        .collect();
                    export_prov_jsonld(&prov_records)
                }
                AuditExportFormat::Json => {
                    let arr: Vec<Value> = records
                        .iter()
                        .map(|r| {
                            json!({
                                "chain": label,
                                "seq": r.seq,
                                "ts": r.entry.ts.to_string(),
                                "actor_chain": r.entry.actor_chain,
                                "action": r.entry.action,
                                "resource": r.entry.resource,
                                "decision": r.entry.decision,
                                "details_json": r.entry.details_json,
                                "prev_hash": hex_encode_32(&r.prev_hash),
                                "hash": hex_encode_32(&r.hash),
                            })
                        })
                        .collect();
                    match serde_json::to_string_pretty(&arr) {
                        Ok(s) => s,
                        Err(err) => {
                            return emit_err(
                                stderr,
                                json_mode,
                                EXIT_RUNTIME_ERROR,
                                &err.to_string(),
                            );
                        }
                    }
                }
            };

            if let Some(dest) = out {
                if let Some(parent) = dest.parent()
                    && !parent.as_os_str().is_empty()
                    && let Err(err) = fs::create_dir_all(parent)
                {
                    return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
                }
                if let Err(err) = fs::write(&dest, &rendered) {
                    return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
                }
                if json_mode {
                    emit_json(
                        stdout,
                        stderr,
                        &json!({
                            "ok": true,
                            "chain": label,
                            "records": records.len(),
                            "path": dest.display().to_string(),
                        }),
                    )
                } else {
                    let _ = writeln!(
                        stdout,
                        "exported {} audit records to `{}`",
                        records.len(),
                        dest.display()
                    );
                    EXIT_OK
                }
            } else {
                let _ = writeln!(stdout, "{rendered}");
                EXIT_OK
            }
        }
    }
}

fn handle_bench(
    cmd: BenchCmdArgs,
    json_mode: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    if cmd.iterations == 0 {
        return emit_err(
            stderr,
            json_mode,
            EXIT_CONFIG_ERROR,
            "`--iterations` must be >= 1",
        );
    }
    let sample_openapi = br#"{"openapi":"3.0.3","info":{"title":"bench","version":"1.0"},"paths":{"/items":{"get":{"operationId":"listItems","responses":{"200":{"description":"ok"}}}}}}"#;
    let extractor = OpenApiExtractor::new();
    let limits = ExtractLimits::default();

    let run_extract = matches!(cmd.suite.as_str(), "all" | "extract");
    let run_diff = matches!(cmd.suite.as_str(), "all" | "diff");
    if !run_extract && !run_diff {
        return emit_err(
            stderr,
            json_mode,
            EXIT_CONFIG_ERROR,
            &format!(
                "unknown bench suite `{}` (expected `all`, `extract`, or `diff`)",
                cmd.suite
            ),
        );
    }

    let mut measurements = JsonMap::new();
    if run_extract {
        let start = Instant::now();
        let mut ops_total = 0usize;
        for _ in 0..cmd.iterations {
            if let Ok(h) = SourceHandle::new(
                Protocol::Rest,
                "bench.openapi.json",
                sample_openapi.to_vec(),
                &limits,
            ) && let Ok(cisg) = extractor.extract(&h, &limits)
            {
                ops_total += cisg.operations.len();
            }
        }
        let elapsed_us = start.elapsed().as_micros();
        measurements.insert(
            "extract".to_owned(),
            json!({
                "iterations": cmd.iterations,
                "operations_extracted": ops_total,
                "total_elapsed_us": elapsed_us,
                "mean_us": elapsed_us / u128::try_from(cmd.iterations).unwrap_or(1),
            }),
        );
    }

    if run_diff {
        let s0 = Snapshot::empty(0);
        let s1 = Snapshot::empty(1);
        let start = Instant::now();
        for _ in 0..cmd.iterations {
            let d = diff_snapshots(&s0, &s1);
            let _ = d.is_empty();
        }
        let elapsed_us = start.elapsed().as_micros();
        measurements.insert(
            "diff".to_owned(),
            json!({
                "iterations": cmd.iterations,
                "total_elapsed_us": elapsed_us,
                "mean_us": elapsed_us / u128::try_from(cmd.iterations).unwrap_or(1),
            }),
        );
    }

    let payload = json!({
        "ok": true,
        "suite": cmd.suite,
        "measurements": Value::Object(measurements),
        "note": "in-process CLI micro-measurement; run `cargo run --release -p gapbench` for full GAP-Bench",
    });

    if let Some(out_dir) = cmd.out {
        if let Err(err) = fs::create_dir_all(&out_dir) {
            return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
        }
        let dest = out_dir.join("cli-microbench.json");
        let pretty = match serde_json::to_string_pretty(&payload) {
            Ok(s) => s,
            Err(err) => return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string()),
        };
        if let Err(err) = fs::write(&dest, pretty) {
            return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
        }
    }

    if json_mode {
        emit_json(stdout, stderr, &payload)
    } else {
        let _ = writeln!(
            stdout,
            "bench suite={} iterations={}",
            cmd.suite, cmd.iterations
        );
        if let Some(obj) = payload.get("measurements").and_then(Value::as_object) {
            for (k, v) in obj {
                let mean = v.get("mean_us").and_then(Value::as_u64).unwrap_or(0);
                let _ = writeln!(stdout, "  {k}: mean={mean} us");
            }
        }
        EXIT_OK
    }
}

fn handle_ingest(
    cmd: IngestCmdArgs,
    json_mode: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let (config_path, spec_override, tenant_raw) = match cmd.sub {
        IngestSubCmd::Run { config, tenant } => (config, None, tenant),
        IngestSubCmd::Once {
            config,
            spec,
            tenant,
        } => (config, spec, tenant),
    };

    let tenant_id = match TenantId::new(&tenant_raw) {
        Ok(t) => t,
        Err(err) => return emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &err.to_string()),
    };

    let pool = WorkerPool::new(WorkerPoolConfig::default());
    let limits = ExtractLimits::default();

    let mut items: Vec<(String, Vec<u8>)> = Vec::new();
    if let Some(spec_path) = spec_override {
        if !spec_path.is_file() {
            return emit_err(
                stderr,
                json_mode,
                EXIT_NOT_FOUND,
                &format!("spec file `{}` not found", spec_path.display()),
            );
        }
        let bytes = match fs::read(&spec_path) {
            Ok(b) => b,
            Err(err) => return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string()),
        };
        items.push((spec_path.display().to_string(), bytes));
    } else {
        let sf = match ServerFile::load(&config_path) {
            Ok(s) => s,
            Err(err) => return emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &err.to_string()),
        };
        for br in &sf.config().branches {
            if let Some(bytes) = sf.openapi_document(&br.id) {
                items.push((br.id.as_str().to_owned(), bytes.to_vec()));
            }
        }
    }

    let mut extracted = Vec::with_capacity(items.len());
    for (name, bytes) in items {
        let label = name.clone();
        let outcome = pool.run_isolated(&tenant_id, None, move || {
            let extractor = OpenApiExtractor::new();
            let handle = SourceHandle::new(Protocol::Rest, &label, bytes, &limits)
                .map_err(|e| e.to_string())?;
            let cisg = extractor
                .extract(&handle, &limits)
                .map_err(|e| e.to_string())?;
            let hash = cisg.cisg_hash().map_err(|e| e.to_string())?;
            Ok::<(usize, usize, usize, String), String>((
                cisg.types.len(),
                cisg.operations.len(),
                cisg.candidate_joins.len(),
                hash,
            ))
        });

        match outcome {
            WorkerOutcome::Succeeded {
                value: (types, ops, joins, cisg_hash),
                ..
            } => extracted.push(json!({
                "source": name,
                "types": types,
                "operations": ops,
                "candidate_joins": joins,
                "cisg_sha256": cisg_hash,
            })),
            WorkerOutcome::Failed { report } => {
                return emit_err(
                    stderr,
                    json_mode,
                    EXIT_RUNTIME_ERROR,
                    &format!("worker failed on `{name}`: {:?}", report.failure),
                );
            }
        }
    }

    if json_mode {
        emit_json(
            stdout,
            stderr,
            &json!({
                "ok": true,
                "tenant": tenant_id.as_str(),
                "extracted": extracted,
            }),
        )
    } else {
        for item in &extracted {
            let src = item.get("source").and_then(Value::as_str).unwrap_or("");
            let ops = item.get("operations").and_then(Value::as_u64).unwrap_or(0);
            let types = item.get("types").and_then(Value::as_u64).unwrap_or(0);
            let hash = item
                .get("cisg_sha256")
                .and_then(Value::as_str)
                .unwrap_or("");
            let _ = writeln!(
                stdout,
                "ingested `{src}`: types={types} operations={ops} cisg_sha256={}...",
                &hash[..12.min(hash.len())]
            );
        }
        EXIT_OK
    }
}

fn handle_tasks(
    cmd: TasksCmdArgs,
    json_mode: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    match cmd.sub {
        TasksSubCmd::List {
            db,
            tenant,
            status,
            limit,
        } => {
            if !db.is_file() {
                return emit_err(
                    stderr,
                    json_mode,
                    EXIT_NOT_FOUND,
                    &format!("task database `{}` not found", db.display()),
                );
            }
            let tid = match TenantId::new(&tenant) {
                Ok(t) => t,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &err.to_string());
                }
            };
            let parsed_status = match status {
                Some(ref raw) => {
                    match TaskStatus::from_wire_str(&raw.trim().to_ascii_uppercase()) {
                        Some(st) => Some(st),
                        None => {
                            return emit_err(
                                stderr,
                                json_mode,
                                EXIT_CONFIG_ERROR,
                                &format!("invalid task status `{raw}`"),
                            );
                        }
                    }
                }
                None => None,
            };
            let store = match SqliteTaskStore::open(&db) {
                Ok(s) => s,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
                }
            };
            let filter = TaskQueryFilter {
                status: parsed_status,
                limit: Some(limit),
            };
            let tasks = match store.list_tasks(&tid.scope(), &filter) {
                Ok(t) => t,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
                }
            };
            if json_mode {
                let rows: Vec<Value> = tasks
                    .iter()
                    .map(|t| {
                        json!({
                            "task_id": t.task_id.to_string(),
                            "status": t.status.as_wire_str(),
                            "lease_owner": t.lease_owner,
                            "updated_at": t.updated_at.to_string(),
                        })
                    })
                    .collect();
                emit_json(
                    stdout,
                    stderr,
                    &json!({ "tenant": tid.as_str(), "tasks": rows }),
                )
            } else {
                for t in &tasks {
                    let _ = writeln!(
                        stdout,
                        "{}\t{}\t{}",
                        t.task_id,
                        t.status.as_wire_str(),
                        t.updated_at
                    );
                }
                EXIT_OK
            }
        }
        TasksSubCmd::Show { id, db, tenant } => {
            if !db.is_file() {
                return emit_err(
                    stderr,
                    json_mode,
                    EXIT_NOT_FOUND,
                    &format!("task database `{}` not found", db.display()),
                );
            }
            let tid = match TenantId::new(&tenant) {
                Ok(t) => t,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &err.to_string());
                }
            };
            let task_id = match gap_core::ids::TaskId::parse(&id) {
                Ok(t) => t,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &err.to_string());
                }
            };
            let store = match SqliteTaskStore::open(&db) {
                Ok(s) => s,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
                }
            };
            let scope = tid.scope();
            let rec = match store.get_task(&scope, task_id) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return emit_err(
                        stderr,
                        json_mode,
                        EXIT_NOT_FOUND,
                        &format!("task `{task_id}` not found for tenant `{}`", tid.as_str()),
                    );
                }
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
                }
            };
            let events = store.events_from(&scope, task_id, 1).unwrap_or_default();
            if json_mode {
                let ev_json: Vec<Value> = events
                    .iter()
                    .map(|e| {
                        json!({
                            "seq": e.seq,
                            "status": e.status.as_wire_str(),
                            "emitted_at": e.emitted_at.to_string(),
                            "payload_json": e.payload_json,
                        })
                    })
                    .collect();
                emit_json(
                    stdout,
                    stderr,
                    &json!({
                        "task_id": rec.task_id.to_string(),
                        "status": rec.status.as_wire_str(),
                        "lease_owner": rec.lease_owner,
                        "updated_at": rec.updated_at.to_string(),
                        "payload_json": rec.payload_json,
                        "events": ev_json,
                    }),
                )
            } else {
                let _ = writeln!(
                    stdout,
                    "task={} status={} updated_at={} events={}",
                    rec.task_id,
                    rec.status.as_wire_str(),
                    rec.updated_at,
                    events.len()
                );
                EXIT_OK
            }
        }
        TasksSubCmd::Cancel {
            id,
            db,
            tenant,
            reason,
        } => {
            if !db.is_file() {
                return emit_err(
                    stderr,
                    json_mode,
                    EXIT_NOT_FOUND,
                    &format!("task database `{}` not found", db.display()),
                );
            }
            let tid = match TenantId::new(&tenant) {
                Ok(t) => t,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &err.to_string());
                }
            };
            let task_id = match gap_core::ids::TaskId::parse(&id) {
                Ok(t) => t,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_CONFIG_ERROR, &err.to_string());
                }
            };
            let store = match SqliteTaskStore::open(&db) {
                Ok(s) => s,
                Err(err) => {
                    return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string());
                }
            };
            let scope = tid.scope();
            let now: Timestamp = SystemClock.now_timestamp();
            let req = TaskTransitionRequest {
                task_id,
                to_status: TaskStatus::Canceled,
                payload_json: None,
                lease_owner: Some(None),
                lease_expires_at: Some(None),
                event_payload_json: json!({ "reason": reason }).to_string(),
                now,
            };
            match store.transition_task(&scope, &req) {
                Ok((rec, ev)) => {
                    if json_mode {
                        emit_json(
                            stdout,
                            stderr,
                            &json!({
                                "ok": true,
                                "task_id": rec.task_id.to_string(),
                                "status": rec.status.as_wire_str(),
                                "seq": ev.seq,
                            }),
                        )
                    } else {
                        let _ = writeln!(stdout, "canceled task {} at seq={}", rec.task_id, ev.seq);
                        EXIT_OK
                    }
                }
                Err(err) => emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string()),
            }
        }
    }
}

fn handle_purge(
    cmd: PurgeCmdArgs,
    json_mode: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    if !cmd.db.is_file() {
        return emit_err(
            stderr,
            json_mode,
            EXIT_NOT_FOUND,
            &format!("context database `{}` not found", cmd.db.display()),
        );
    }
    let clock = Arc::new(SystemClock);
    let store = match SqliteContextStore::open(SqliteConfig::new(&cmd.db), clock) {
        Ok(s) => s,
        Err(err) => return emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string()),
    };
    let policy = PurgePolicy {
        task_retention: Duration::from_secs(cmd.retention_secs),
    };
    match store.purge_expired(&policy) {
        Ok(report) => {
            if json_mode {
                emit_json(
                    stdout,
                    stderr,
                    &json!({
                        "ok": true,
                        "purged_ephemeral_nodes": report.purged_ephemeral_nodes,
                        "purged_task_nodes": report.purged_task_nodes,
                        "purged_edges": report.purged_edges,
                    }),
                )
            } else {
                let _ = writeln!(
                    stdout,
                    "purged ephemeral_nodes={} task_nodes={} edges={}",
                    report.purged_ephemeral_nodes, report.purged_task_nodes, report.purged_edges
                );
                EXIT_OK
            }
        }
        Err(err) => emit_err(stderr, json_mode, EXIT_RUNTIME_ERROR, &err.to_string()),
    }
}

fn handle_completions(
    cmd: CompletionsCmdArgs,
    json_mode: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    let command = FullCli::command();
    let script = render_completions(&command, cmd.shell);
    if json_mode {
        emit_json(
            stdout,
            stderr,
            &json!({
                "shell": format!("{:?}", cmd.shell).to_ascii_lowercase(),
                "script": script,
            }),
        )
    } else {
        let _ = write!(stdout, "{script}");
        EXIT_OK
    }
}

/// Generates shell completion scripts from the live `clap::Command` definition (`OPS-044`).
#[must_use]
pub fn render_completions(cmd: &Command, shell: ShellKind) -> String {
    let bin = cmd.get_name();
    let top_subs: Vec<&str> = cmd
        .get_subcommands()
        .map(Command::get_name)
        .filter(|n| *n != "help")
        .collect();
    let joined = top_subs.join(" ");
    let mut out = String::new();

    match shell {
        ShellKind::Bash => {
            let _ = writeln!(out, "# bash completion for {bin}");
            let _ = writeln!(out, "_{bin}_completions() {{");
            let _ = writeln!(out, "  local cur prev");
            let _ = writeln!(out, "  cur=\"${{COMP_WORDS[COMP_CWORD]}}\"");
            let _ = writeln!(out, "  prev=\"${{COMP_WORDS[1]}}\"");
            let _ = writeln!(out, "  if [[ $COMP_CWORD -eq 1 ]]; then");
            let _ = writeln!(
                out,
                "    COMPREPLY=( $(compgen -W \"{joined} --json --help --version\" -- \"$cur\") )"
            );
            let _ = writeln!(out, "    return 0");
            let _ = writeln!(out, "  fi");
            let _ = writeln!(out, "  case \"$prev\" in");
            for sub in cmd.get_subcommands().filter(|s| s.get_name() != "help") {
                let sub_words: Vec<String> = sub
                    .get_subcommands()
                    .map(|s| s.get_name().to_owned())
                    .chain(
                        sub.get_arguments()
                            .filter_map(|a| a.get_long().map(|l| format!("--{l}"))),
                    )
                    .collect();
                let _ = writeln!(
                    out,
                    "    {}) COMPREPLY=( $(compgen -W \"{}\" -- \"$cur\") ) ;;",
                    sub.get_name(),
                    sub_words.join(" ")
                );
            }
            let _ = writeln!(out, "  esac");
            let _ = writeln!(out, "}}");
            let _ = writeln!(out, "complete -F _{bin}_completions {bin}");
        }
        ShellKind::Zsh => {
            let _ = writeln!(out, "#compdef {bin}");
            let _ = writeln!(out, "_{bin}() {{");
            let _ = writeln!(out, "  local -a commands");
            let _ = writeln!(out, "  commands=(");
            for sub in cmd.get_subcommands().filter(|s| s.get_name() != "help") {
                let desc = sub
                    .get_about()
                    .map(|s| s.to_string().replace('\'', "''"))
                    .unwrap_or_default();
                let _ = writeln!(out, "    '{}:{}'", sub.get_name(), desc);
            }
            let _ = writeln!(out, "  )");
            let _ = writeln!(out, "  _describe 'command' commands");
            let _ = writeln!(out, "}}");
            let _ = writeln!(out, "_{bin} \"$@\"");
        }
        ShellKind::Fish => {
            let _ = writeln!(out, "# fish completion for {bin}");
            let _ = writeln!(out, "complete -c {bin} -l json -d 'Emit JSON output'");
            for sub in cmd.get_subcommands().filter(|s| s.get_name() != "help") {
                let desc = sub
                    .get_about()
                    .map(|s| s.to_string().replace('\'', "\\'"))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "complete -c {bin} -f -n '__fish_use_subcommand' -a '{}' -d '{}'",
                    sub.get_name(),
                    desc
                );
                for nested in sub.get_subcommands() {
                    let _ = writeln!(
                        out,
                        "complete -c {bin} -f -n '__fish_seen_subcommand_from {}' -a '{}'",
                        sub.get_name(),
                        nested.get_name()
                    );
                }
            }
        }
        ShellKind::PowerShell => {
            let _ = writeln!(out, "# PowerShell completion for {bin}");
            let _ = writeln!(
                out,
                "Register-ArgumentCompleter -Native -CommandName '{bin}' -ScriptBlock {{"
            );
            let _ = writeln!(
                out,
                "  param($wordToComplete, $commandAst, $cursorPosition)"
            );
            let _ = writeln!(
                out,
                "  @({}) | Where-Object {{ $_ -like \"$wordToComplete*\" }} | ForEach-Object {{ [System.Management.Automation.CompletionResult]::new($_, $_, 'ParameterValue', $_) }}",
                top_subs
                    .iter()
                    .map(|s| format!("'{s}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let _ = writeln!(out, "}}");
        }
    }
    out
}
