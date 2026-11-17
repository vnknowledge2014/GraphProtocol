//! Dual-stack HTTP/3 over QUIC (`RFC 9114`, TLS 1.3 ALPN `h3`) transport listener and client
//! (`OPS-005`, `OPS-002`, `RUN-090`, `INV-02`, `INV-15`, `ADR-026`).
//!
//! Enforces TLS 1.3 with `0-RTT` early data disabled (`max_early_data_size = 0`) so non-idempotent
//! operations cannot be replayed, caps active QUIC connections via an atomic semaphore, enforces
//! per-stream request body limits (`413 Payload Too Large`), streams HTTP/3 response bodies without
//! buffering (`graphql-sse`), and drains active connections within `shutdown_grace`.

use std::fmt;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use bytes::{Buf, Bytes, BytesMut};
use futures_util::future;
use http::header::{CONNECTION, HeaderName, TRANSFER_ENCODING, UPGRADE};
use http::{Request, Response, StatusCode};
use http_body_util::BodyExt as _;
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use tokio::sync::Semaphore;
use tokio::task::{JoinHandle, JoinSet};
use tower::ServiceExt as _;

use crate::serve::ServeOptions;

/// Default maximum request body size buffered from an HTTP/3 request stream (`1 MiB`).
pub const DEFAULT_HTTP3_MAX_BODY_BYTES: usize = 1_048_576;

/// Validated TLS 1.3 QUIC server configuration (`ALPN = h3`, `0-RTT` disabled, `OPS-005`).
#[derive(Clone)]
pub struct Http3TlsConfig {
    quic: Arc<QuicServerConfig>,
}

impl fmt::Debug for Http3TlsConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Http3TlsConfig")
            .field("alpn", &"h3")
            .field("zero_rtt", &false)
            .finish()
    }
}

impl Http3TlsConfig {
    /// Parses PEM-encoded certificate chain and private key bytes and builds a TLS 1.3 QUIC config
    /// with `ALPN = ["h3"]` and `max_early_data_size = 0` (`OPS-005`, `INV-15`).
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if PEM decoding fails, no certificates are present,
    /// or the keypair cannot configure TLS 1.3.
    pub fn from_pem(cert_pem: &[u8], key_pem: &[u8]) -> io::Result<Self> {
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| invalid(&format!("invalid HTTP/3 TLS certificate PEM: {err}")))?;
        if certs.is_empty() {
            return Err(invalid(
                "HTTP/3 TLS certificate PEM contained no certificates",
            ));
        }

        let key = PrivateKeyDer::from_pem_slice(key_pem)
            .map_err(|err| invalid(&format!("invalid HTTP/3 TLS private key PEM: {err}")))?;

        let provider = Arc::new(quinn::rustls::crypto::ring::default_provider());
        let mut tls_config = quinn::rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&quinn::rustls::version::TLS13])
            .map_err(|err| invalid(&format!("failed to configure TLS 1.3 for HTTP/3: {err}")))?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|err| invalid(&format!("invalid HTTP/3 TLS keypair: {err}")))?;

        // `OPS-005`: Disable TLS 1.3 0-RTT early data so mutations and MCP tool calls cannot replay.
        tls_config.max_early_data_size = 0;
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        let quic = QuicServerConfig::try_from(tls_config).map_err(|err| {
            invalid(&format!(
                "failed to construct QUIC server TLS config: {err}"
            ))
        })?;
        Ok(Self {
            quic: Arc::new(quic),
        })
    }

    /// Generates an ephemeral self-signed PEM `(cert_pem, key_pem)` pair strictly for development
    /// or test fixtures (`OPS-005`).
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if `alt_names` is empty or certificate generation fails.
    pub fn self_signed_pem_for_dev(alt_names: &[String]) -> io::Result<(String, String)> {
        if alt_names.is_empty() {
            return Err(invalid(
                "self_signed_pem_for_dev requires at least one SAN entry",
            ));
        }
        let certified = rcgen::generate_simple_self_signed(alt_names.to_vec())
            .map_err(|err| invalid(&format!("failed to generate dev HTTP/3 TLS cert: {err}")))?;
        Ok((certified.cert.pem(), certified.signing_key.serialize_pem()))
    }

    /// Generates an ephemeral self-signed certificate for `alt_names` strictly for `profile = "dev"`
    /// (`OPS-005`) and returns `(Http3TlsConfig, cert_der_bytes)`.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] if `alt_names` is empty or certificate generation fails.
    pub fn self_signed_for_dev(alt_names: &[String]) -> io::Result<(Self, Vec<u8>)> {
        if alt_names.is_empty() {
            return Err(invalid(
                "self_signed_for_dev requires at least one SAN entry",
            ));
        }
        let certified = rcgen::generate_simple_self_signed(alt_names.to_vec())
            .map_err(|err| invalid(&format!("failed to generate dev HTTP/3 TLS cert: {err}")))?;
        let cert_der = certified.cert.der().as_ref().to_vec();
        let cert_pem = certified.cert.pem();
        let key_pem = certified.signing_key.serialize_pem();
        let tls = Self::from_pem(cert_pem.as_bytes(), key_pem.as_bytes())?;
        Ok((tls, cert_der))
    }
}

/// Hardening knobs for [`serve_http3`] (`OPS-005`, `RUN-090`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Http3ServeOptions {
    /// Connection cap, handshake timeout, and shutdown grace period shared with TCP transport.
    pub serve: ServeOptions,
    /// Maximum incoming HTTP/3 request body size per stream in bytes.
    pub max_request_body_bytes: usize,
}

impl Default for Http3ServeOptions {
    fn default() -> Self {
        Self {
            serve: ServeOptions::default(),
            max_request_body_bytes: DEFAULT_HTTP3_MAX_BODY_BYTES,
        }
    }
}

impl Http3ServeOptions {
    /// Validates all HTTP/3 listener options.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::InvalidInput`] for invalid nested [`ServeOptions`] or zero
    /// `max_request_body_bytes`.
    pub fn validate(&self) -> io::Result<()> {
        self.serve.validate()?;
        if self.max_request_body_bytes == 0 {
            return Err(invalid("max_request_body_bytes must be greater than zero"));
        }
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.to_owned())
}

fn strip_rfc9114_connection_headers(headers: &mut http::HeaderMap) {
    let keep_alive = HeaderName::from_static("keep-alive");
    let proxy_connection = HeaderName::from_static("proxy-connection");
    headers.remove(CONNECTION);
    headers.remove(&keep_alive);
    headers.remove(&proxy_connection);
    headers.remove(TRANSFER_ENCODING);
    headers.remove(UPGRADE);
}

async fn read_bounded_request_body<S, B>(
    stream: &mut h3::server::RequestStream<S, B>,
    max_bytes: usize,
) -> Result<Result<Bytes, StatusCode>, h3::error::StreamError>
where
    S: h3::quic::RecvStream,
    B: Buf,
{
    let mut out = BytesMut::new();
    while let Some(mut chunk) = stream.recv_data().await? {
        let chunk_len = chunk.remaining();
        if out.len().saturating_add(chunk_len) > max_bytes {
            return Ok(Err(StatusCode::PAYLOAD_TOO_LARGE));
        }
        while chunk.has_remaining() {
            let slice = chunk.chunk();
            out.extend_from_slice(slice);
            let consumed = slice.len();
            chunk.advance(consumed);
        }
    }
    Ok(Ok(out.freeze()))
}

async fn handle_http3_stream<S>(
    resolver: h3::server::RequestResolver<S, Bytes>,
    app: Router,
    max_body_bytes: usize,
) where
    S: h3::quic::Connection<Bytes>,
    S::BidiStream: h3::quic::SendStream<Bytes> + h3::quic::RecvStream,
{
    let (parts, mut stream) = match resolver.resolve_request().await {
        Ok((req, stream)) => (req.into_parts().0, stream),
        Err(err) => {
            tracing::debug!(%err, "failed resolving HTTP/3 request headers");
            return;
        }
    };

    let body_bytes = match read_bounded_request_body(&mut stream, max_body_bytes).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(status)) => {
            if let Ok(resp) = Response::builder().status(status).body(()) {
                let _ = stream.send_response(resp).await;
                let _ = stream.finish().await;
            }
            return;
        }
        Err(err) => {
            tracing::debug!(%err, "failed reading HTTP/3 request body");
            return;
        }
    };

    let request = Request::from_parts(parts, Body::from(body_bytes));
    let response = match app.oneshot(request).await {
        Ok(resp) => resp,
        Err(infallible) => match infallible {},
    };

    let (mut resp_parts, mut resp_body) = response.into_parts();
    strip_rfc9114_connection_headers(&mut resp_parts.headers);

    let head = Response::from_parts(resp_parts, ());
    if let Err(err) = stream.send_response(head).await {
        tracing::debug!(%err, "failed sending HTTP/3 response headers");
        return;
    }

    while let Some(frame_res) = resp_body.frame().await {
        match frame_res {
            Ok(frame) => {
                if let Ok(data) = frame.into_data()
                    && !data.is_empty()
                    && let Err(err) = stream.send_data(data).await
                {
                    tracing::debug!(%err, "failed streaming HTTP/3 response body frame");
                    return;
                }
            }
            Err(err) => {
                tracing::debug!(%err, "error polling axum response body over HTTP/3");
                return;
            }
        }
    }

    if let Err(err) = stream.finish().await {
        tracing::debug!(%err, "failed finishing HTTP/3 response stream");
    }
}

async fn handle_http3_connection(
    connecting: quinn::Incoming,
    app: Router,
    header_read_timeout: Duration,
    max_body_bytes: usize,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    let _permit = permit;
    let Ok(accept_res) = connecting.accept() else {
        return;
    };
    let quinn_conn = match tokio::time::timeout(header_read_timeout, accept_res).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(err)) => {
            tracing::debug!(%err, "QUIC TLS handshake failed");
            return;
        }
        Err(_) => {
            tracing::debug!(timeout = ?header_read_timeout, "QUIC TLS handshake timed out");
            return;
        }
    };

    let mut h3_conn: h3::server::Connection<h3_quinn::Connection, Bytes> =
        match h3::server::Connection::new(h3_quinn::Connection::new(quinn_conn)).await {
            Ok(conn) => conn,
            Err(err) => {
                tracing::debug!(%err, "HTTP/3 server connection initialization failed");
                return;
            }
        };

    let mut streams = JoinSet::new();
    loop {
        tokio::select! {
            _ = streams.join_next(), if !streams.is_empty() => {}
            accepted = h3_conn.accept() => {
                match accepted {
                    Ok(Some(resolver)) => {
                        let app_clone = app.clone();
                        streams.spawn(async move {
                            handle_http3_stream(resolver, app_clone, max_body_bytes).await;
                        });
                    }
                    Ok(None) => break,
                    Err(err) => {
                        tracing::debug!(%err, "HTTP/3 connection accept closed");
                        break;
                    }
                }
            }
        }
    }

    while streams.join_next().await.is_some() {}
}

/// Serves `app` over QUIC + HTTP/3 (`RFC 9114`, `OPS-005`) on `udp_socket` until `shutdown`
/// resolves, then stops accepting new connections and drains in-flight streams for at most
/// `options.serve.shutdown_grace` (`RUN-090`).
///
/// # Errors
///
/// Returns an [`io::Error`] if `options` fail validation or `udp_socket` cannot be configured or
/// bound to the async runtime.
pub async fn serve_http3(
    udp_socket: std::net::UdpSocket,
    tls: Http3TlsConfig,
    app: Router,
    shutdown: impl Future<Output = ()> + Send,
    options: Http3ServeOptions,
) -> io::Result<()> {
    options.validate()?;
    udp_socket.set_nonblocking(true)?;

    let mut transport_cfg = quinn::TransportConfig::default();
    transport_cfg.max_concurrent_bidi_streams(256_u32.into());
    transport_cfg.max_concurrent_uni_streams(64_u32.into());

    let mut server_config = quinn::ServerConfig::with_crypto(tls.quic);
    server_config.transport_config(Arc::new(transport_cfg));

    let runtime = quinn::default_runtime()
        .ok_or_else(|| io::Error::other("no async UDP runtime available for HTTP/3"))?;
    let endpoint = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config),
        udp_socket,
        runtime,
    )?;

    let permits = Arc::new(Semaphore::new(options.serve.max_connections));
    let mut conn_tasks = JoinSet::new();

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = conn_tasks.join_next(), if !conn_tasks.is_empty() => {}
            incoming_opt = endpoint.accept() => {
                let Some(incoming) = incoming_opt else {
                    break;
                };
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    tracing::warn!(max = options.serve.max_connections, "HTTP/3 connection cap reached: refusing QUIC handshake");
                    incoming.refuse();
                    continue;
                };
                let app_clone = app.clone();
                let timeout = options.serve.header_read_timeout;
                let max_body = options.max_request_body_bytes;
                conn_tasks.spawn(async move {
                    handle_http3_connection(incoming, app_clone, timeout, max_body, permit).await;
                });
            }
        }
    }

    // Wait gracefully for in-flight HTTP/3 connections within `shutdown_grace`, then close endpoint.
    let drain = async { while conn_tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(options.serve.shutdown_grace, drain)
        .await
        .is_err()
    {
        tracing::warn!(
            grace = ?options.serve.shutdown_grace,
            "HTTP/3 shutdown grace elapsed with connections still open"
        );
    }
    endpoint.close(0_u32.into(), b"draining");
    endpoint.wait_idle().await;
    Ok(())
}

/// Active HTTP/3 response body stream returned by [`Http3Client::open_stream`].
pub struct Http3ResponseStream {
    inner: h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>,
}

impl fmt::Debug for Http3ResponseStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Http3ResponseStream")
            .field("stream_id", &self.inner.id())
            .finish()
    }
}

impl Http3ResponseStream {
    /// Reads the next chunk from the HTTP/3 response body, returning `Ok(None)` on EOF.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if the HTTP/3 stream fails or resets.
    pub async fn next_chunk(&mut self) -> io::Result<Option<Bytes>> {
        let chunk_opt = self
            .inner
            .recv_data()
            .await
            .map_err(|err| io::Error::other(format!("HTTP/3 recv_data failed: {err}")))?;
        let Some(mut buf) = chunk_opt else {
            return Ok(None);
        };
        let mut out = BytesMut::with_capacity(buf.remaining());
        while buf.has_remaining() {
            let slice = buf.chunk();
            out.extend_from_slice(slice);
            let n = slice.len();
            buf.advance(n);
        }
        Ok(Some(out.freeze()))
    }
}

/// Asynchronous HTTP/3 over QUIC client used for end-to-end verification and federated hops (`OPS-005`).
pub struct Http3Client {
    send_request: h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    endpoint: quinn::Endpoint,
    driver_task: JoinHandle<()>,
}

impl fmt::Debug for Http3Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Http3Client").finish_non_exhaustive()
    }
}

impl Drop for Http3Client {
    fn drop(&mut self) {
        self.driver_task.abort();
        self.endpoint.close(0_u32.into(), b"client_drop");
    }
}

impl Http3Client {
    /// Connects to a dual-stack HTTP/3 server at `server_addr` trusting `trusted_cert_der` (`SNI = server_name`).
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if TLS verification, UDP socket binding, or QUIC/HTTP/3 handshake fails.
    pub async fn connect_pinned_der(
        server_addr: SocketAddr,
        server_name: &str,
        trusted_cert_der: &[u8],
    ) -> io::Result<Self> {
        let mut roots = quinn::rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(trusted_cert_der.to_vec()))
            .map_err(|err| invalid(&format!("invalid pinned DER certificate: {err}")))?;

        let provider = Arc::new(quinn::rustls::crypto::ring::default_provider());
        let mut tls_client = quinn::rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&quinn::rustls::version::TLS13])
            .map_err(|err| invalid(&format!("TLS 1.3 client setup failed: {err}")))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls_client.alpn_protocols = vec![b"h3".to_vec()];
        tls_client.enable_early_data = false;

        let quic_client = QuicClientConfig::try_from(tls_client)
            .map_err(|err| invalid(&format!("QUIC client TLS config failed: {err}")))?;

        let bind_addr: SocketAddr = if server_addr.is_ipv4() {
            SocketAddr::from(([127, 0, 0, 1], 0))
        } else {
            SocketAddr::from(([0_u16; 8], 0))
        };
        let mut endpoint = quinn::Endpoint::client(bind_addr)?;
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(quic_client)));

        let connecting = endpoint
            .connect(server_addr, server_name)
            .map_err(|err| io::Error::other(format!("QUIC connect error: {err}")))?;
        let conn = connecting
            .await
            .map_err(|err| io::Error::other(format!("QUIC handshake failed: {err}")))?;

        let (mut driver, send_request) = h3::client::new(h3_quinn::Connection::new(conn))
            .await
            .map_err(|err| io::Error::other(format!("HTTP/3 client setup failed: {err}")))?;

        let driver_task = tokio::spawn(async move {
            let _ = future::poll_fn(|cx| driver.poll_close(cx)).await;
        });

        Ok(Self {
            send_request,
            endpoint,
            driver_task,
        })
    }

    /// Sends an HTTP/3 request and opens the response stream for incremental consumption (`graphql-sse`).
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] if sending headers/body or awaiting initial response headers fails.
    pub async fn open_stream(
        &mut self,
        request: Request<Bytes>,
    ) -> io::Result<(Response<()>, Http3ResponseStream)> {
        let (parts, body) = request.into_parts();
        let head = Request::from_parts(parts, ());
        let mut req_stream = self
            .send_request
            .send_request(head)
            .await
            .map_err(|err| io::Error::other(format!("HTTP/3 send_request failed: {err}")))?;
        if !body.is_empty() {
            req_stream
                .send_data(body)
                .await
                .map_err(|err| io::Error::other(format!("HTTP/3 send_data failed: {err}")))?;
        }
        req_stream
            .finish()
            .await
            .map_err(|err| io::Error::other(format!("HTTP/3 finish request failed: {err}")))?;
        let response = req_stream
            .recv_response()
            .await
            .map_err(|err| io::Error::other(format!("HTTP/3 recv_response failed: {err}")))?;
        Ok((response, Http3ResponseStream { inner: req_stream }))
    }

    /// Sends a unary HTTP/3 request and collects its full response body.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] on stream or protocol error.
    pub async fn request(&mut self, request: Request<Bytes>) -> io::Result<(Response<()>, Bytes)> {
        let (response, mut stream) = self.open_stream(request).await?;
        let mut buf = BytesMut::new();
        while let Some(chunk) = stream.next_chunk().await? {
            buf.extend_from_slice(&chunk);
        }
        Ok((response, buf.freeze()))
    }
}
