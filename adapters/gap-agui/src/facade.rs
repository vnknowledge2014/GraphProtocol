//! Inbound [`AgUiFacade`] implementing `gap_core::traits::HttpModule` (`L4`, `CMP-025`..`CMP-028`).

use std::sync::Arc;

use gap_core::context::RequestContext;
use gap_core::error::{ErrorCode, GapError, GapResult};
use gap_core::snapshot::TrustTier;
use gap_core::traits::{
    BoxFuture, DiscoveryRequest, DiscoveryService, GraphqlEngine, GraphqlRequest, GraphqlResponse,
    HttpExchangeRequest, HttpExchangeResponse, HttpModule,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::events::{
    AG_UI_PROTOCOL_VERSION, AgUiEvent, AgUiInterrupt, EventType, RunOutcome, encode_sse_stream,
};
use crate::input::{InterruptResume, RunAgentInput, extract_gapql, extract_user_intent};

/// Default maximum inbound `POST /ag-ui` JSON body size (`512 KiB`).
pub const MAX_AGUI_REQUEST_BYTES: usize = 512 * 1024;

const CAPABILITIES_PATH: &str = "/ag-ui/capabilities";

/// Runtime configuration for [`AgUiFacade`] (`CMP-025`..`CMP-028`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgUiConfig {
    /// Maximum accepted JSON request body in bytes.
    pub max_request_bytes: usize,
    /// Wrap outputs derived from `T2` branches inside escaped `<gap:untrusted>` envelopes (`CMP-028`).
    pub spotlight: bool,
    /// Also spotlight `T1` branches when `spotlight` is `true`.
    pub spotlight_t1: bool,
}

impl Default for AgUiConfig {
    fn default() -> Self {
        Self {
            max_request_bytes: MAX_AGUI_REQUEST_BYTES,
            spotlight: true,
            spotlight_t1: false,
        }
    }
}

/// Inbound AG-UI v1.0 `HttpModule` mounted by `gap-server` at `/ag-ui` (`ADR-027`).
#[derive(Debug, Clone)]
pub struct AgUiFacade {
    engine: Arc<dyn GraphqlEngine>,
    discovery: Option<Arc<dyn DiscoveryService>>,
    config: AgUiConfig,
}

impl AgUiFacade {
    /// Constructs a new [`AgUiFacade`] over GAP's deterministic [`GraphqlEngine`].
    #[must_use]
    pub fn new(engine: Arc<dyn GraphqlEngine>) -> Self {
        Self {
            engine,
            discovery: None,
            config: AgUiConfig::default(),
        }
    }

    /// Attaches optional calibrated [`DiscoveryService`] for intent-only runs (`ARC-002`).
    #[must_use]
    pub fn with_discovery(mut self, discovery: Arc<dyn DiscoveryService>) -> Self {
        self.discovery = Some(discovery);
        self
    }

    /// Overrides facade configuration.
    #[must_use]
    pub const fn with_config(mut self, config: AgUiConfig) -> Self {
        self.config = config;
        self
    }

    fn capabilities_response(&self) -> GapResult<HttpExchangeResponse> {
        let body = serde_json::to_vec(&json!({
            "protocol": "ag-ui",
            "version": AG_UI_PROTOCOL_VERSION,
            "mountPath": "/ag-ui",
            "transport": "sse",
            "interrupts": true,
            "streamCompaction": true,
            "spotlighting": self.config.spotlight,
            "supportedEvents": [
                EventType::RunStarted,
                EventType::StepStarted,
                EventType::ToolCallStart,
                EventType::ToolCallArgs,
                EventType::ToolCallEnd,
                EventType::ToolCallResult,
                EventType::StateSnapshot,
                EventType::StateDelta,
                EventType::TextMessageStart,
                EventType::TextMessageContent,
                EventType::TextMessageEnd,
                EventType::StepFinished,
                EventType::RunFinished,
                EventType::RunError,
            ],
            "protocolAxes": ["MCP 2026-07-28", "A2A 1.0", "AG-UI 1.0"]
        }))
        .map_err(|err| GapError::new(ErrorCode::Internal, err.to_string()))?;

        Ok(HttpExchangeResponse {
            status: 200,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body,
        })
    }

    async fn handle_post(
        &self,
        ctx: &RequestContext,
        req: &HttpExchangeRequest,
    ) -> HttpExchangeResponse {
        if req.body.len() > self.config.max_request_bytes {
            return plain_json_error(
                413,
                "GAP_RESPONSE_TOO_LARGE",
                "AG-UI request body exceeds maximum allowed size",
            );
        }

        let input: RunAgentInput = match serde_json::from_slice(&req.body) {
            Ok(parsed) => parsed,
            Err(err) => {
                return plain_json_error(
                    400,
                    "GAP_PARSE_ERROR",
                    &format!("invalid AG-UI RunAgentInput JSON: {err}"),
                );
            }
        };

        let thread_id = if input.thread_id.trim().is_empty() {
            Uuid::now_v7().to_string()
        } else {
            input.thread_id.clone()
        };
        let run_id = if input.run_id.trim().is_empty() {
            Uuid::now_v7().to_string()
        } else {
            input.run_id.clone()
        };
        let now_ms = ctx.request_id.get_timestamp().map(|ts| {
            let (secs, subsec_nanos) = ts.to_unix();
            secs.saturating_mul(1000)
                .saturating_add(u64::from(subsec_nanos) / 1_000_000)
        });
        let message_id = format!("msg_{run_id}");
        let generation = ctx.snapshot.generation;

        let mut events: Vec<AgUiEvent> = Vec::with_capacity(12);
        let mut start_ev =
            AgUiEvent::run_started(&thread_id, &run_id, input.parent_run_id.clone(), now_ms);
        let mut base_meta = Map::new();
        base_meta.insert("generation".to_owned(), json!(generation));
        base_meta.insert("tenant".to_owned(), json!(ctx.tenant.as_str()));
        start_ev.metadata = Some(base_meta.clone());
        events.push(start_ev);

        // If client explicitly denied an approval interrupt, end run safely without executing.
        if let Some(denied) = input.resume.iter().find(|r| {
            r.status.eq_ignore_ascii_case("deny") || r.status.eq_ignore_ascii_case("denied")
        }) {
            events.push(AgUiEvent::step_started("gap.interrupt_resume", now_ms));
            events.push(AgUiEvent::state_snapshot(
                json!({
                    "generation": generation,
                    "resumedInterruptId": denied.interrupt_id,
                    "decision": "denied",
                    "executed": false
                }),
                now_ms,
            ));
            let deny_msg = format!(
                "Interrupt `{}` was denied; GAP did not execute any backend mutation.",
                denied.interrupt_id
            );
            push_text_message(&mut events, &message_id, &deny_msg, now_ms);
            events.push(AgUiEvent::step_finished("gap.interrupt_resume", now_ms));
            events.push(AgUiEvent::run_finished(
                &thread_id,
                &run_id,
                RunOutcome::Success,
                Some(json!({ "status": "cancelled_by_user" })),
                now_ms,
            ));
            return sse_ok_response(&events);
        }

        // Handle explicit GAP-QL query/mutation execution (including approved interrupt resume).
        if let Some(mut gql_req) = extract_gapql(&input) {
            if let Some(approved) = input.resume.iter().find(|r| {
                r.status.eq_ignore_ascii_case("approve")
                    || r.status.eq_ignore_ascii_case("approved")
            }) {
                inject_approval_extension(&mut gql_req, approved);
            }
            return self
                .execute_gapql_run(
                    ctx,
                    &input,
                    &gql_req,
                    &thread_id,
                    &run_id,
                    &message_id,
                    now_ms,
                    &base_meta,
                    events,
                )
                .await;
        }

        // Advisory discovery when only natural language messages exist (ARC-002: never mutates).
        if let (Some(discovery), Some(intent)) =
            (self.discovery.as_ref(), extract_user_intent(&input))
        {
            return self
                .execute_discovery_run(
                    ctx,
                    discovery,
                    &intent,
                    &thread_id,
                    &run_id,
                    &message_id,
                    now_ms,
                    &base_meta,
                    events,
                )
                .await;
        }

        // Guidance fallback returning pinned L0 branch summaries.
        events.push(AgUiEvent::step_started("gap.overview", now_ms));
        let branch_names: Vec<&str> = ctx
            .snapshot
            .branches
            .iter()
            .filter(|(_, b)| self.config.spotlight || b.trust != TrustTier::T2)
            .map(|(_, b)| b.id.as_str())
            .collect();
        events.push(AgUiEvent::state_snapshot(
            json!({
                "generation": generation,
                "protocol": "GAP-QL over AG-UI 1.0",
                "branches": branch_names,
            }),
            now_ms,
        ));
        let help = format!(
            "GAP gateway ready (generation {generation}, {} visible branches). Supply `forwardedProps.gapql` for deterministic execution or send a natural-language user intent.",
            ctx.snapshot.branches.len()
        );
        push_text_message(&mut events, &message_id, &help, now_ms);
        events.push(AgUiEvent::step_finished("gap.overview", now_ms));
        events.push(AgUiEvent::run_finished(
            &thread_id,
            &run_id,
            RunOutcome::Success,
            Some(json!({ "generation": generation })),
            now_ms,
        ));
        sse_ok_response(&events)
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_gapql_run(
        &self,
        ctx: &RequestContext,
        input: &RunAgentInput,
        gql_req: &GraphqlRequest,
        thread_id: &str,
        run_id: &str,
        message_id: &str,
        now_ms: Option<u64>,
        base_meta: &Map<String, Value>,
        mut events: Vec<AgUiEvent>,
    ) -> HttpExchangeResponse {
        events.push(AgUiEvent::step_started("gap.execute", now_ms));
        let tool_call_id = format!("tc_{run_id}");
        events.push(AgUiEvent::tool_call_start(
            &tool_call_id,
            "gap_query",
            Some(message_id.to_owned()),
            now_ms,
        ));
        if let Ok(args_str) = serde_json::to_string(gql_req) {
            events.push(AgUiEvent::tool_call_args(&tool_call_id, args_str, now_ms));
        }
        events.push(AgUiEvent::tool_call_end(&tool_call_id, now_ms));

        let resp = self.engine.execute(ctx, gql_req.clone()).await;

        // Check whether deterministic Cedar / side-effect gate paused for human approval (`CMP-027`).
        if let Some(interrupt) = detect_approval_interrupt(&resp, run_id) {
            events.push(AgUiEvent::state_snapshot(
                json!({
                    "generation": ctx.snapshot.generation,
                    "status": "input_required",
                    "pendingInterrupt": interrupt,
                    "threadId": thread_id,
                }),
                now_ms,
            ));
            events.push(AgUiEvent::step_finished("gap.execute", now_ms));
            let mut fin = AgUiEvent::run_finished(
                thread_id,
                run_id,
                RunOutcome::Interrupt {
                    interrupts: vec![interrupt],
                },
                None,
                now_ms,
            );
            fin.metadata = Some(base_meta.clone());
            events.push(fin);
            return sse_ok_response(&events);
        }

        let spotlight_text =
            render_response_text(ctx, &resp, self.config.spotlight, self.config.spotlight_t1);
        events.push(AgUiEvent::tool_call_result(
            format!("tmsg_{run_id}"),
            &tool_call_id,
            spotlight_text.clone(),
            now_ms,
        ));

        let mut next_state = match &input.state {
            Value::Object(m) => m.clone(),
            _ => Map::new(),
        };
        next_state.insert("generation".to_owned(), json!(ctx.snapshot.generation));
        if let Some(data) = &resp.data {
            next_state.insert("lastData".to_owned(), data.clone());
        }
        if !resp.errors.is_empty() {
            next_state.insert("errors".to_owned(), json!(resp.errors));
        }
        events.push(AgUiEvent::state_snapshot(Value::Object(next_state), now_ms));
        push_text_message(&mut events, message_id, &spotlight_text, now_ms);
        events.push(AgUiEvent::step_finished("gap.execute", now_ms));

        let mut fin = AgUiEvent::run_finished(
            thread_id,
            run_id,
            RunOutcome::Success,
            resp.data.clone(),
            now_ms,
        );
        let mut fin_meta = base_meta.clone();
        fin_meta.insert("errorCount".to_owned(), json!(resp.errors.len()));
        fin.metadata = Some(fin_meta);
        events.push(fin);

        sse_ok_response(&events)
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_discovery_run(
        &self,
        ctx: &RequestContext,
        discovery: &Arc<dyn DiscoveryService>,
        intent: &str,
        thread_id: &str,
        run_id: &str,
        message_id: &str,
        now_ms: Option<u64>,
        base_meta: &Map<String, Value>,
        mut events: Vec<AgUiEvent>,
    ) -> HttpExchangeResponse {
        events.push(AgUiEvent::step_started("gap.discover", now_ms));
        let tool_call_id = format!("tc_{run_id}");
        events.push(AgUiEvent::tool_call_start(
            &tool_call_id,
            "gap_search",
            Some(message_id.to_owned()),
            now_ms,
        ));
        events.push(AgUiEvent::tool_call_args(
            &tool_call_id,
            json!({ "query": intent, "limit": 5 }).to_string(),
            now_ms,
        ));
        events.push(AgUiEvent::tool_call_end(&tool_call_id, now_ms));

        let dreq = DiscoveryRequest {
            intent: intent.to_owned(),
            top_k: 5,
            scopes: Vec::new(),
        };
        match discovery.resolve(ctx, dreq).await {
            Ok(res) => {
                let rendered = serde_json::to_string(&res).unwrap_or_else(|_| "{}".to_owned());
                let safe_rendered = if self.config.spotlight {
                    format!(
                        "<gap:untrusted source=\"discovery/{}\">{}</gap:untrusted>",
                        res.router,
                        escape_untrusted_tags(&rendered)
                    )
                } else {
                    rendered
                };
                events.push(AgUiEvent::tool_call_result(
                    format!("tmsg_{run_id}"),
                    &tool_call_id,
                    safe_rendered.clone(),
                    now_ms,
                ));
                events.push(AgUiEvent::state_snapshot(
                    json!({
                        "generation": ctx.snapshot.generation,
                        "router": res.router,
                        "outcome": res.outcome,
                        "candidates": res.candidates,
                    }),
                    now_ms,
                ));
                push_text_message(&mut events, message_id, &safe_rendered, now_ms);
                events.push(AgUiEvent::step_finished("gap.discover", now_ms));
                let mut fin = AgUiEvent::run_finished(
                    thread_id,
                    run_id,
                    RunOutcome::Success,
                    serde_json::to_value(&res).ok(),
                    now_ms,
                );
                fin.metadata = Some(base_meta.clone());
                events.push(fin);
                sse_ok_response(&events)
            }
            Err(err) => {
                events.push(AgUiEvent::run_error(
                    err.message(),
                    format!("{:?}", err.code()),
                    now_ms,
                ));
                events.push(AgUiEvent::step_finished("gap.discover", now_ms));
                sse_ok_response(&events)
            }
        }
    }
}

impl HttpModule for AgUiFacade {
    fn module_name(&self) -> &'static str {
        "ag-ui"
    }

    fn mount_prefixes(&self) -> &'static [&'static str] {
        &["/ag-ui"]
    }

    fn handle<'a>(
        &'a self,
        ctx: &'a RequestContext,
        req: HttpExchangeRequest,
    ) -> BoxFuture<'a, GapResult<HttpExchangeResponse>> {
        Box::pin(async move {
            let method = req.method.to_ascii_uppercase();
            if method == "GET" && req.path == CAPABILITIES_PATH {
                return self.capabilities_response();
            }
            if method == "POST" && (req.path == "/ag-ui" || req.path == "/ag-ui/run") {
                return Ok(self.handle_post(ctx, &req).await);
            }
            Ok(HttpExchangeResponse {
                status: 405,
                headers: vec![("allow".to_owned(), "GET, POST".to_owned())],
                body: b"method not allowed for AG-UI endpoint".to_vec(),
            })
        })
    }
}

fn push_text_message(
    events: &mut Vec<AgUiEvent>,
    message_id: &str,
    content: &str,
    timestamp: Option<u64>,
) {
    events.push(AgUiEvent::text_message_start(
        message_id,
        "assistant",
        timestamp,
    ));
    events.push(AgUiEvent::text_message_content(
        message_id, content, timestamp,
    ));
    events.push(AgUiEvent::text_message_end(message_id, timestamp));
}

fn inject_approval_extension(req: &mut GraphqlRequest, resume: &InterruptResume) {
    let mut approval = Map::new();
    approval.insert("interruptId".to_owned(), json!(resume.interrupt_id));
    approval.insert("approved".to_owned(), json!(true));
    if !resume.payload.is_null() {
        approval.insert("payload".to_owned(), resume.payload.clone());
    }
    let gap_ext = req
        .extensions
        .entry("gap".to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(map) = gap_ext {
        map.insert("approval".to_owned(), Value::Object(approval));
    }
}

fn detect_approval_interrupt(resp: &GraphqlResponse, run_id: &str) -> Option<AgUiInterrupt> {
    if let Some(err) = resp
        .errors
        .iter()
        .find(|e| e.code() == ErrorCode::ApprovalRequired || e.code() == ErrorCode::AuthRequired)
    {
        return Some(AgUiInterrupt {
            id: format!("int_{run_id}"),
            reason: if err.code() == ErrorCode::ApprovalRequired {
                "approval_required".to_owned()
            } else {
                "auth_required".to_owned()
            },
            payload: json!({
                "message": err.message(),
                "code": err.code(),
                "extensions": err.extensions,
            }),
        });
    }
    if let Some(status) = resp
        .extensions
        .get("gap")
        .and_then(|g| g.get("taskState"))
        .and_then(Value::as_str)
        && status == "INPUT_REQUIRED"
    {
        return Some(AgUiInterrupt {
            id: format!("int_{run_id}"),
            reason: "input_required".to_owned(),
            payload: resp.extensions.get("gap").cloned().unwrap_or(Value::Null),
        });
    }
    None
}

fn render_response_text(
    ctx: &RequestContext,
    resp: &GraphqlResponse,
    spotlight: bool,
    spotlight_t1: bool,
) -> String {
    let raw = serde_json::to_string(resp).unwrap_or_else(|_| "{}".to_owned());
    if !spotlight {
        return raw;
    }
    let mut untrusted_sources: Vec<String> = Vec::new();
    for (_, branch) in ctx.snapshot.branches.iter() {
        let should = match branch.trust {
            TrustTier::T2 => true,
            TrustTier::T1 => spotlight_t1,
            TrustTier::T0 => false,
        };
        if should {
            untrusted_sources.push(branch.id.as_str().to_owned());
        }
    }
    if untrusted_sources.is_empty() {
        return raw;
    }
    let source_label = untrusted_sources.join(",");
    format!(
        "<gap:untrusted source=\"{source_label}\">{}</gap:untrusted>",
        escape_untrusted_tags(&raw)
    )
}

/// Escapes any literal `<gap:untrusted` or `</gap:untrusted` delimiters in untrusted text (`CMP-028`).
#[must_use]
pub(crate) fn escape_untrusted_tags(raw: &str) -> String {
    const OPEN: &[u8] = b"<gap:untrusted";
    const CLOSE: &[u8] = b"</gap:untrusted";

    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    let mut last = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let tail = &bytes[i..];
            let hit = (tail.len() >= OPEN.len() && tail[..OPEN.len()].eq_ignore_ascii_case(OPEN))
                || (tail.len() >= CLOSE.len() && tail[..CLOSE.len()].eq_ignore_ascii_case(CLOSE));
            if hit {
                out.push_str(&raw[last..i]);
                out.push_str("\\u003c");
                i += 1;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    if last < raw.len() {
        out.push_str(&raw[last..]);
    }
    out
}

fn sse_ok_response(events: &[AgUiEvent]) -> HttpExchangeResponse {
    HttpExchangeResponse {
        status: 200,
        headers: vec![
            (
                "content-type".to_owned(),
                "text/event-stream; charset=utf-8".to_owned(),
            ),
            ("cache-control".to_owned(), "no-cache".to_owned()),
        ],
        body: encode_sse_stream(events),
    }
}

fn plain_json_error(status: u16, code: &str, message: &str) -> HttpExchangeResponse {
    let body = serde_json::to_vec(&json!({ "code": code, "message": message }))
        .unwrap_or_else(|_| b"{}".to_vec());
    HttpExchangeResponse {
        status,
        headers: vec![("content-type".to_owned(), "application/json".to_owned())],
        body,
    }
}
