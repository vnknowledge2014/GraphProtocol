//! Production listener for the transport router (`OPS-001`, `RUN-090`, `INV-02`).
//!
//! `axum::serve` has no header-read timeout, no connection cap and no bounded drain: a connection that
//! sends half a request line is held open forever (found by the `WP-M1-08` review). [`serve`] fixes that:
//! `hyper` is configured with a header-read timeout, new connections beyond `max_connections` are closed at
//! once (never queued), and shutdown stops accepting, lets in-flight requests finish for `shutdown_grace`
//! and then returns.

use std::future::Future;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::Request as AxumRequest;
use axum::middleware::{self, Next};
use axum::response::Response as AxumResponse;
use http::header::{HeaderName, HeaderValue};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

/// Default time a client may take to send a complete request head.
pub const DEFAULT_HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Default cap on simultaneously open connections.
pub const DEFAULT_MAX_CONNECTIONS: usize = 10_000;
/// Default drain period after a shutdown signal (`RUN-090` `shutdown.grace`).
pub const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// Listener hardening knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeOptions {
    /// A connection that has not delivered a complete request head within this time is closed.
    pub header_read_timeout: Duration,
    /// Connections beyond this number are closed immediately after `accept`.
    pub max_connections: usize,
    /// How long in-flight requests may keep running after the shutdown signal.
    pub shutdown_grace: Duration,
    /// When set, TCP HTTP/1.1 & HTTP/2 responses advertise `Alt-Svc: h3=":<port>"; ma=86400` (`OPS-005`).
    pub alt_svc_h3_port: Option<u16>,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            header_read_timeout: DEFAULT_HEADER_READ_TIMEOUT,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
            alt_svc_h3_port: None,
        }
    }
}

impl ServeOptions {
    /// Checks the knobs are usable.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] for a zero timeout or a zero connection cap.
    pub fn validate(&self) -> io::Result<()> {
        if self.header_read_timeout.is_zero() {
            return Err(invalid("header_read_timeout must be greater than zero"));
        }
        if self.max_connections == 0 {
            return Err(invalid("max_connections must be at least 1"));
        }
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

/// Serves `app` on `listener` until `shutdown` completes, then drains for at most
/// `options.shutdown_grace`.
///
/// # Errors
///
/// Returns an error only for invalid `options`; per-connection failures are logged and never stop the server.
pub async fn serve(
    listener: TcpListener,
    app: Router,
    shutdown: impl Future<Output = ()> + Send,
    options: ServeOptions,
) -> io::Result<()> {
    options.validate()?;
    let app = if let Some(port) = options.alt_svc_h3_port {
        let header_val = HeaderValue::from_str(&format!("h3=\":{port}\"; ma=86400"))
            .map_err(|err| invalid(&format!("invalid Alt-Svc header value: {err}")))?;
        let alt_svc_name = HeaderName::from_static("alt-svc");
        app.layer(middleware::from_fn(move |req: AxumRequest, next: Next| {
            let val = header_val.clone();
            let name = alt_svc_name.clone();
            async move {
                let mut resp: AxumResponse = next.run(req).await;
                if !resp.headers().contains_key(&name) {
                    resp.headers_mut().insert(name, val);
                }
                resp
            }
        }))
    } else {
        app
    };
    let permits = Arc::new(Semaphore::new(options.max_connections));
    let graceful = GracefulShutdown::new();

    let mut builder = Builder::new(TokioExecutor::new());
    builder
        .http1()
        .timer(TokioTimer::new())
        .header_read_timeout(options.header_read_timeout)
        .keep_alive(true);
    builder.http2().timer(TokioTimer::new());

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _peer)) => stream,
                    Err(error) => {
                        // `EMFILE` and friends are transient: back off instead of spinning.
                        tracing::warn!(%error, "accept failed");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    tracing::warn!(max = options.max_connections, "connection cap reached: closing a new connection");
                    drop(stream);
                    continue;
                };
                let connection = builder
                    .serve_connection_with_upgrades(
                        TokioIo::new(stream),
                        TowerToHyperService::new(app.clone()),
                    )
                    .into_owned();
                let connection = graceful.watch(connection);
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) = connection.await {
                        tracing::debug!(%error, "connection closed with an error");
                    }
                });
            }
        }
    }

    drop(listener);
    tokio::select! {
        () = graceful.shutdown() => {}
        () = tokio::time::sleep(options.shutdown_grace) => {
            tracing::warn!(grace = ?options.shutdown_grace, "shutdown grace elapsed with requests still in flight");
        }
    }
    Ok(())
}
