//! HTTP listener, `stdio` MCP server, structured `stderr` logging, and two-stage graceful
//! [`Shutdown`] (`RUN-090`, `OPS-001`, `OPS-002`, `OPS-011`, `SEC-004`, `INV-01`).

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use gap_core::config::{Profile, TelemetryConfig};
use gap_core::context::{Budget, Principal, PrincipalKind, RequestContext, TraceCtx};
use gap_core::error::GapError;
use gap_mcp::inbound::{ContextFactory, StdioServer};
use gap_transport::ServeOptions;
use gap_transport::config::HARD_MAX_DEADLINE;
use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tracing::instrument::WithSubscriber as _;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use uuid::Uuid;

use crate::assemble::{AssembleError, Assembled, BranchBuildError, ServerFile, assemble_default};

/// Exit code for clean server shutdown (`OPS-044`, `Brief §3.4`).
pub const EXIT_OK: i32 = 0;

/// Exit code for runtime failure (connector `Describe`, bind, I/O, or `stdio` transport error).
pub const EXIT_RUNTIME_ERROR: i32 = 1;

/// Exit code for CLI usage or configuration validation failure (`OPS-044`, `Brief §3.4`).
pub const EXIT_CONFIG_ERROR: i32 = 2;

/// Exit code emitted when a second `SIGTERM`/`SIGINT` forces immediate termination (`RUN-090`).
pub const FORCE_SHUTDOWN_EXIT_CODE: i32 = 130;

/// Bounded capacity for internal OS signal forwarding (`Rule 6`: bounded channels only).
const SIGNAL_CHANNEL_CAPACITY: usize = 8;

/// Runtime error returned by [`serve_http`] or [`serve_stdio`].
#[derive(Debug, Error)]
pub enum RunError {
    /// Listener or signal setup I/O failure.
    #[error("server I/O error: {0}")]
    Io(#[from] io::Error),
    /// Inbound `stdio` MCP failure.
    #[error("stdio MCP server error: {0}")]
    Stdio(#[from] GapError),
    /// Second shutdown signal received during drain (`RUN-090`).
    #[error("forced immediate shutdown on second signal (exit code {exit_code})")]
    ForcedShutdown {
        /// Non-zero exit code requested (`130`).
        exit_code: i32,
    },
}

/// Future resolved when the first shutdown signal initiates graceful draining (`RUN-090`).
#[derive(Debug)]
pub struct ShutdownTrigger {
    rx: oneshot::Receiver<()>,
}

impl Future for ShutdownTrigger {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Ready(_) => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Callback invoked on a second shutdown signal (`std::process::exit(130)` in production).
pub type ForceExitFn = Arc<dyn Fn(i32) + Send + Sync>;

/// Two-stage `SIGTERM`/`SIGINT` shutdown controller (`RUN-090`, `Brief §3.3`).
pub struct Shutdown {
    draining: Arc<AtomicBool>,
    signals: mpsc::Receiver<()>,
    on_force_exit: ForceExitFn,
}

impl fmt::Debug for Shutdown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shutdown")
            .field("draining", &self.draining)
            .finish_non_exhaustive()
    }
}

impl Shutdown {
    /// Constructs a [`Shutdown`] controller driven by an explicit bounded `signals` channel and
    /// `on_force_exit` hook (used by deterministic socket-free tests).
    #[must_use]
    pub fn with_signals(
        draining: Arc<AtomicBool>,
        signals: mpsc::Receiver<()>,
        on_force_exit: ForceExitFn,
    ) -> Self {
        Self {
            draining,
            signals,
            on_force_exit,
        }
    }

    /// Subscribes to OS `SIGTERM` and `SIGINT` signals (`RUN-090`).
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if OS signal registration fails.
    pub fn os(draining: Arc<AtomicBool>) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel(SIGNAL_CHANNEL_CAPACITY);
        spawn_os_signal_forwarder(tx)?;
        Ok(Self {
            draining,
            signals: rx,
            on_force_exit: Arc::new(|code| std::process::exit(code)),
        })
    }

    /// Runs `serve_fn` with two-stage shutdown supervision:
    /// - First signal sets `draining = true` immediately (flipping `/readyz` to `503`) and resolves
    ///   [`ShutdownTrigger`].
    /// - Second signal during drain invokes `on_force_exit(130)` and returns
    ///   [`RunError::ForcedShutdown`].
    ///
    /// # Errors
    ///
    /// Returns [`RunError`] if `serve_fn` fails or a second signal forces immediate exit.
    pub async fn drive<F, Fut>(mut self, serve_fn: F) -> Result<(), RunError>
    where
        F: FnOnce(ShutdownTrigger) -> Fut,
        Fut: Future<Output = io::Result<()>>,
    {
        let (trigger_tx, trigger_rx) = oneshot::channel();
        let serve_fut = serve_fn(ShutdownTrigger { rx: trigger_rx });
        tokio::pin!(serve_fut);

        tokio::select! {
            res = &mut serve_fut => {
                return res.map_err(RunError::Io);
            }
            sig = self.signals.recv() => {
                if sig.is_none() {
                    return serve_fut.await.map_err(RunError::Io);
                }
                self.draining.store(true, Ordering::SeqCst);
                tracing::info!("received first shutdown signal; gateway draining (/readyz -> 503)");
                let _ = trigger_tx.send(());
            }
        }

        tokio::select! {
            res = &mut serve_fut => {
                tracing::info!("graceful HTTP drain completed");
                res.map_err(RunError::Io)
            }
            sig = self.signals.recv() => {
                if sig.is_some() {
                    tracing::warn!(
                        exit_code = FORCE_SHUTDOWN_EXIT_CODE,
                        "received second shutdown signal during drain; forcing immediate exit"
                    );
                    (self.on_force_exit)(FORCE_SHUTDOWN_EXIT_CODE);
                    Err(RunError::ForcedShutdown {
                        exit_code: FORCE_SHUTDOWN_EXIT_CODE,
                    })
                } else {
                    serve_fut.await.map_err(RunError::Io)
                }
            }
        }
    }
}

#[cfg(unix)]
fn spawn_os_signal_forwarder(tx: mpsc::Sender<()>) -> io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::spawn(async move {
        loop {
            let received = tokio::select! {
                r = sigterm.recv() => r.is_some(),
                r = sigint.recv() => r.is_some(),
            };
            if !received || tx.send(()).await.is_err() {
                break;
            }
        }
    });
    Ok(())
}

#[cfg(not(unix))]
fn spawn_os_signal_forwarder(tx: mpsc::Sender<()>) -> io::Result<()> {
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            if tx.send(()).await.is_err() {
                break;
            }
        }
    });
    Ok(())
}

/// Builds a `tracing::Dispatch` writing structured logs to `make_writer` (`OPS-011`).
///
/// Logs never include secrets or raw queries (`INV-15`, `OPS-011`).
#[must_use]
pub fn make_tracing_dispatch<W>(telemetry: &TelemetryConfig, make_writer: W) -> tracing::Dispatch
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let filter =
        EnvFilter::try_new(&telemetry.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    if telemetry.log_json {
        let sub = tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_writer(make_writer)
            .with_ansi(false)
            .finish();
        tracing::Dispatch::new(sub)
    } else {
        let sub = tracing_subscriber::fmt()
            .compact()
            .with_env_filter(filter)
            .with_writer(make_writer)
            .with_ansi(false)
            .finish();
        tracing::Dispatch::new(sub)
    }
}

/// Initialises process-wide `tracing-subscriber` formatting to `stderr` (`OPS-011`, `Brief §3.3`).
///
/// Safe to call more than once: if a global subscriber is already installed, subsequent calls are
/// no-ops.
pub fn init_tracing(telemetry: &TelemetryConfig) {
    let dispatch = make_tracing_dispatch(telemetry, io::stderr);
    let _ = tracing::dispatcher::set_global_default(dispatch);
}

fn os_service_principal_id() -> String {
    let raw = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "stdio-service".to_owned()
    } else {
        format!("stdio:{cleaned}")
    }
}

fn resolve_stdio_scopes(assembled: &Assembled) -> BTreeSet<String> {
    let default_tenant = &assembled.config.gateway.default_tenant;
    let mut scopes = BTreeSet::new();
    for key in &assembled.config.api_keys {
        if &key.tenant == default_tenant {
            scopes.extend(key.scopes.iter().cloned());
        }
    }
    if scopes.is_empty()
        && let Some(first) = assembled.config.api_keys.first()
    {
        scopes.extend(first.scopes.iter().cloned());
    }
    if scopes.is_empty() && assembled.config.profile == Profile::Dev {
        scopes.insert("read".to_owned());
    }
    scopes
}

/// Builds the per-request [`ContextFactory`] for `gap serve --stdio` (`SEC-004`, `INV-01`).
///
/// Each invocation pins a fresh [`Snapshot`](gap_core::snapshot::Snapshot) from
/// [`Assembled::snapshot_handle`], applies `[limits]` (`max_cost_units` and `request_deadline_ms`),
/// and authenticates the local OS caller as a [`PrincipalKind::Service`] principal (`SEC-004`).
#[must_use]
pub fn build_stdio_context_factory(assembled: &Assembled) -> ContextFactory {
    let snapshot_handle = Arc::clone(&assembled.snapshot_handle);
    let tenant = assembled.config.gateway.default_tenant.clone();
    let max_cost_units = assembled.config.limits.max_cost_units;
    let deadline_dur =
        Duration::from_millis(assembled.config.limits.request_deadline_ms).min(HARD_MAX_DEADLINE);

    let mut base_principal = Principal::new(
        os_service_principal_id(),
        PrincipalKind::Service,
        tenant.clone(),
    );
    base_principal.scopes = resolve_stdio_scopes(assembled);

    Arc::new(move || {
        let snapshot = snapshot_handle.pin();
        RequestContext::try_new(
            Uuid::now_v7(),
            tenant.clone(),
            base_principal.clone(),
            Instant::now() + deadline_dur,
            Budget::new(max_cost_units),
            TraceCtx::generate_root(true),
            0,
            None,
            snapshot,
        )
    })
}

/// Serves `assembled` over `listener` with OS `SIGTERM`/`SIGINT` graceful shutdown (`RUN-090`).
///
/// # Errors
///
/// Returns [`RunError`] if signal setup, listener options, or forced shutdown fails.
pub async fn serve_http(assembled: Assembled, listener: TcpListener) -> Result<(), RunError> {
    let shutdown = Shutdown::os(Arc::clone(&assembled.draining))?;
    let app = gap_transport::router(Arc::clone(&assembled.app_state));
    let options = ServeOptions {
        shutdown_grace: Duration::from_millis(assembled.config.server.shutdown_grace_ms),
        ..ServeOptions::default()
    };
    tracing::info!(
        generation = assembled.snapshot_handle.generation(),
        shutdown_grace_ms = assembled.config.server.shutdown_grace_ms,
        "starting GAP HTTP server"
    );
    let _keep_alive = assembled;
    shutdown
        .drive(|trigger| gap_transport::serve(listener, app, trigger, options))
        .await
}

/// Serves `assembled`'s [`gap_mcp::inbound::McpFacade`] over custom `reader`, `writer`, and `log_writer`
/// streams (`SEC-004`, `INV-01`, `OPS-011`).
///
/// All structured `tracing` events emitted while serving are routed exclusively to `log_writer`,
/// keeping `writer` (`stdout`) reserved strictly for JSON-RPC protocol frames.
///
/// # Errors
///
/// Returns [`RunError`] on unrecoverable `stdio` read/write failure.
pub async fn serve_stdio_with_streams<R, W, L>(
    assembled: &Assembled,
    reader: R,
    writer: W,
    log_writer: L,
) -> Result<(), RunError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
    L: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let dispatch = make_tracing_dispatch(&assembled.config.telemetry, log_writer);
    let server = StdioServer::new(
        assembled.facade.clone(),
        build_stdio_context_factory(assembled),
    );
    let generation = assembled.snapshot_handle.generation();
    let branch_count = assembled.config.branches.len();

    async move {
        tracing::info!(
            mode = "stdio",
            generation,
            branches = branch_count,
            "starting GAP MCP stdio server"
        );
        server.serve(reader, writer).await.map_err(RunError::Stdio)
    }
    .with_subscriber(dispatch)
    .await
}

/// Serves `assembled`'s [`gap_mcp::inbound::McpFacade`] over custom `reader` and `writer` with logs on
/// `stderr` (`SEC-004`, `INV-01`, `OPS-011`).
///
/// # Errors
///
/// Returns [`RunError`] on unrecoverable `stdio` read/write failure.
pub async fn serve_stdio_with_io<R, W>(
    assembled: &Assembled,
    reader: R,
    writer: W,
) -> Result<(), RunError>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    serve_stdio_with_streams(assembled, reader, writer, io::stderr).await
}

/// Serves `assembled`'s [`gap_mcp::inbound::McpFacade`] over `stdin`/`stdout` (`SEC-004`, `INV-01`).
///
/// # Errors
///
/// Returns [`RunError`] on unrecoverable `stdio` read/write failure.
pub async fn serve_stdio(assembled: &Assembled) -> Result<(), RunError> {
    serve_stdio_with_io(
        assembled,
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    )
    .await
}

const fn classify_assemble_exit_code(err: &AssembleError) -> i32 {
    match err {
        AssembleError::Config(_)
        | AssembleError::Transport(_)
        | AssembleError::Facade(_)
        | AssembleError::Composition { .. }
        | AssembleError::Branch(BranchBuildError::SecretResolution { .. }) => EXIT_CONFIG_ERROR,
        AssembleError::Branch(BranchBuildError::Describe { .. })
        | AssembleError::Branch(BranchBuildError::ConnectorInit { .. })
        | AssembleError::SnapshotPublish(_) => EXIT_RUNTIME_ERROR,
    }
}

/// Loads `config_path`, initialises `stderr` logging, assembles the gateway, and serves either
/// `stdio` MCP (`stdio == true`) or HTTP (`stdio == false`), returning the standardized process
/// exit code (`0` clean shutdown, `1` runtime failure, `2` config/usage error).
pub async fn run_server_from_path(config_path: &Path, stdio: bool) -> i32 {
    let server_file = match ServerFile::load(config_path) {
        Ok(sf) => sf,
        Err(err) => {
            let _ = io::Write::write_fmt(&mut io::stderr(), format_args!("error: {err}\n"));
            return EXIT_CONFIG_ERROR;
        }
    };

    let listen_addr: Option<SocketAddr> = if stdio {
        None
    } else {
        match server_file.config().server.listen.parse::<SocketAddr>() {
            Ok(addr) => Some(addr),
            Err(err) => {
                let _ = io::Write::write_fmt(
                    &mut io::stderr(),
                    format_args!(
                        "error: invalid `server.listen` socket address `{}`: {err}\n",
                        server_file.config().server.listen
                    ),
                );
                return EXIT_CONFIG_ERROR;
            }
        }
    };

    init_tracing(&server_file.config().telemetry);

    let assembled = match assemble_default(&server_file).await {
        Ok(asm) => asm,
        Err(err) => {
            let code = classify_assemble_exit_code(&err);
            tracing::error!(error = %err, "failed to assemble GAP gateway");
            let _ = io::Write::write_fmt(&mut io::stderr(), format_args!("error: {err}\n"));
            return code;
        }
    };

    for warning in &assembled.warnings {
        tracing::warn!(warning = ?warning, "snapshot composition warning");
    }

    if let Some(addr) = listen_addr {
        let listener = match TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(err) => {
                tracing::error!(%addr, error = %err, "failed to bind HTTP listener");
                let _ = io::Write::write_fmt(
                    &mut io::stderr(),
                    format_args!("error: failed to bind `{addr}`: {err}\n"),
                );
                return EXIT_RUNTIME_ERROR;
            }
        };
        match serve_http(assembled, listener).await {
            Ok(()) => EXIT_OK,
            Err(RunError::ForcedShutdown { exit_code }) => exit_code,
            Err(err) => {
                let _ = io::Write::write_fmt(&mut io::stderr(), format_args!("error: {err}\n"));
                EXIT_RUNTIME_ERROR
            }
        }
    } else {
        match serve_stdio(&assembled).await {
            Ok(()) => EXIT_OK,
            Err(err) => {
                let _ = io::Write::write_fmt(&mut io::stderr(), format_args!("error: {err}\n"));
                EXIT_RUNTIME_ERROR
            }
        }
    }
}
