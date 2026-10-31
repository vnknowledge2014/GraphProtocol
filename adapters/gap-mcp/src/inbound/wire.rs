//! HTTP and `stdio` JSON-RPC wire handling (`CMP-001`, `INV-01`, `INV-15`).

use gap_core::context::RequestContext;
use gap_core::error::GapResult;
use gap_core::traits::{HttpExchangeRequest, HttpExchangeResponse};
use tokio::io::{AsyncBufRead, AsyncWrite};

use super::{ContextFactory, FacadeInner};

pub(crate) async fn handle_http(
    _inner: &FacadeInner,
    _ctx: &RequestContext,
    _req: HttpExchangeRequest,
) -> GapResult<HttpExchangeResponse> {
    Ok(HttpExchangeResponse {
        status: 501,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body: b"{}".to_vec(),
    })
}

pub(crate) async fn serve_stdio<R, W>(
    _inner: &FacadeInner,
    _ctx_factory: &ContextFactory,
    _reader: R,
    _writer: W,
) -> GapResult<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    Ok(())
}
