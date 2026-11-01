//! [`GraphqlEngine`] over the transport pipeline (`ARC-007`, `INV-01`, `INV-03`): protocol facades (MCP, A2A)
//! execute GAP-QL through the same `Operation -> Authz -> Plan -> Exec` stack as `/graphql`, under a
//! [`RequestContext`] the server already authenticated and pinned (the `TransportMux` and `Gateway` layers
//! are the HTTP front half and are not repeated).

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use gap_core::context::RequestContext;
use gap_core::error::{ErrorCode, GapError};
use gap_core::telemetry::tenant_bucket;
use gap_core::traits::{BoxFuture, GraphqlEngine, GraphqlRequest, GraphqlResponse};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;
use tower::{Layer, Service};

use crate::app::AppState;
use crate::operation::NegotiatedMedia;
use crate::pipeline::{
    AuthzLayer, OperationLayer, OperationRequest, PipelineError, PipelineOutput, PlanLayer,
    TransportExecService,
};

/// Runs GAP-QL documents for protocol facades (`GraphqlEngine`).
#[derive(Clone)]
pub struct TransportEngine {
    state: Arc<AppState>,
}

impl fmt::Debug for TransportEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransportEngine").finish_non_exhaustive()
    }
}

impl TransportEngine {
    /// Wraps the shared transport state (plan cache, connector registry, limits, clock).
    #[must_use]
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }
}

impl GraphqlEngine for TransportEngine {
    fn execute<'a>(
        &'a self,
        ctx: &'a RequestContext,
        req: GraphqlRequest,
    ) -> BoxFuture<'a, GraphqlResponse> {
        Box::pin(run(Arc::clone(&self.state), ctx, req))
    }
}

async fn run(
    state: Arc<AppState>,
    ctx: &RequestContext,
    mut req: GraphqlRequest,
) -> GraphqlResponse {
    clamp_deadline_to_context(ctx, &mut req);
    let body = match serde_json::to_vec(&req) {
        Ok(bytes) => Bytes::from(bytes),
        Err(_) => {
            return error_response(GapError::new(ErrorCode::Internal, "cannot encode request"));
        }
    };

    // Dropping this future (the facade gave up, the client went away) cancels the connector calls (`INV-12`).
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();

    let request_span = tracing::info_span!(
        "gap.request",
        gap.generation = ctx.snapshot.generation,
        gap.operation_name = tracing::field::Empty,
        gap.tenant = u64::from(tenant_bucket(&ctx.tenant)),
        gap.principal_kind = ctx.principal.kind.as_str(),
        gap.cost.planned = tracing::field::Empty,
        request_id = %ctx.request_id,
        trace_id = %ctx.trace.trace_id_hex(),
    );

    let exec = TransportExecService {
        state: Arc::clone(&state),
    };
    let plan = PlanLayer {
        state: Arc::clone(&state),
    }
    .layer(exec);
    let authz = AuthzLayer {
        state: Arc::clone(&state),
    }
    .layer(plan);
    let mut operation = OperationLayer { state }.layer(authz);

    let result = operation
        .call(OperationRequest {
            request_id: ctx.request_id.to_string(),
            trace: ctx.trace.clone(),
            media: NegotiatedMedia::GraphqlResponseJson,
            request_span,
            principal: ctx.principal.clone(),
            snapshot: Arc::clone(&ctx.snapshot),
            is_get: false,
            raw_uri_query: None,
            body_bytes: body,
            cancel,
        })
        .await;
    response_from(result)
}

/// The pipeline derives the request deadline from `extensions.gap.deadlineMs` (`RUN-030`): the facade's own
/// deadline (`ctx.deadline`) must never be exceeded, so the smaller of the two wins.
fn clamp_deadline_to_context(ctx: &RequestContext, req: &mut GraphqlRequest) {
    let remaining_ms = ctx
        .deadline
        .saturating_duration_since(std::time::Instant::now())
        .as_millis()
        .max(1);
    let remaining_ms = u64::try_from(remaining_ms).unwrap_or(u64::MAX);
    let gap = req
        .extensions
        .entry("gap")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(gap) = gap {
        let requested = gap.get("deadlineMs").and_then(Value::as_u64);
        gap.insert(
            "deadlineMs".to_owned(),
            Value::from(requested.map_or(remaining_ms, |r| r.min(remaining_ms))),
        );
    }
}

fn response_from(result: Result<PipelineOutput, PipelineError>) -> GraphqlResponse {
    match result {
        Ok(PipelineOutput::Assembled(payload)) => parse_wire_body(&payload.body_bytes)
            .unwrap_or_else(|| {
                error_response(GapError::new(
                    ErrorCode::Internal,
                    "the pipeline produced a response that is not a GraphQL object",
                ))
            }),
        Ok(PipelineOutput::Sse(_)) => error_response(GapError::new(
            ErrorCode::Internal,
            "unexpected event stream from the pipeline",
        )),
        Err(PipelineError::GraphqlRequest { errors, .. }) => GraphqlResponse {
            data: None,
            errors,
            extensions: Map::new(),
        },
        Err(PipelineError::Transport { error, .. }) => error_response(error),
    }
}

fn error_response(error: GapError) -> GraphqlResponse {
    GraphqlResponse {
        data: None,
        errors: vec![error],
        extensions: Map::new(),
    }
}

fn parse_wire_body(bytes: &[u8]) -> Option<GraphqlResponse> {
    let Value::Object(mut object) = serde_json::from_slice::<Value>(bytes).ok()? else {
        return None;
    };
    let data = match object.remove("data") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value),
    };
    let errors = match object.remove("errors") {
        Some(Value::Array(items)) => items.iter().map(gap_error_from_wire).collect(),
        _ => Vec::new(),
    };
    let extensions = match object.remove("extensions") {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    Some(GraphqlResponse {
        data,
        errors,
        extensions,
    })
}

/// Rebuilds a [`GapError`] from a serialized response error; anything unparseable becomes `GAP_INTERNAL`
/// (the response already left the executor: nothing here may panic or leak).
fn gap_error_from_wire(value: &Value) -> GapError {
    if let Ok(error) = serde_json::from_value::<GapError>(value.clone()) {
        return error;
    }
    let code = value["extensions"]["code"]
        .as_str()
        .and_then(|c| c.parse::<ErrorCode>().ok())
        .unwrap_or(ErrorCode::Internal);
    let message = value["message"].as_str().unwrap_or("request failed");
    GapError::new(code, message)
}
