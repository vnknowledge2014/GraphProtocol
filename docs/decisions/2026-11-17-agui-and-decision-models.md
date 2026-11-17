# Context Memo — AG-UI v1.0 Event-Stream Facade & Multi-Provider System-1 Decision Models (`ADR-027`, `ADR-028`)

- **Date:** 2026-11-17
- **Work Package:** `WP-M7-13`
- **Status:** Accepted (`ADR-027`, `ADR-028`)
- **Spec Anchors:** `CMP-025`..`CMP-028`, `RTE-047`..`RTE-049`, `ARC-002`, `ARC-007`, `INV-01`, `INV-04`, `INV-09`, `INV-15`, `INV-17`

## 1. Why AG-UI v1.0 Alongside MCP & A2A (`ADR-027`)

GAP already unifies:
- **Vertical agent-to-tool / data invocation:** Inbound/outbound **MCP `2026-07-28`** (`/mcp`, `gap-mcp`).
- **Horizontal agent-to-agent delegation:** Inbound/outbound **A2A `v1.0`** (`/.well-known/agent-card.json`, `/a2a`, `gap-a2a`).

Modern agent frontends also require **agent-to-UI state & interaction streaming** standardized by the **Agent-User Interaction (`AG-UI`) v1.0** protocol (`ag-ui-protocol/ag-ui`). Without an inbound AG-UI facade, frontend applications must write bespoke GraphQL/MCP/A2A polling glue and ad-hoc approval modals.

Adding `adapters/gap-agui` (`Layer L4`, implementing `gap_core::traits::HttpModule` mounted at `/ag-ui`) completes all three open agent protocol axes on a single gateway over both TCP HTTP/1.1+2 (`Alt-Svc`) and UDP HTTP/3 QUIC (`OPS-005`), while keeping every GAP safety invariant intact:
- Every `POST /ag-ui` run pins one `Arc<Snapshot>` (`INV-01`) and enforces Cedar policies (`INV-04`) before executing typed `GAP-QL`.
- Human-in-the-loop approvals triggered by `@sideEffect(IRREVERSIBLE | UNKNOWN)` (`INPUT_REQUIRED`) map deterministically to AG-UI `RUN_FINISHED` with `outcome: { type: "interrupt", interrupts: [...] }` and resume via `RunAgentInput.resume[]` (`CMP-027`).
- Untrusted branch text inside `TOOL_CALL_RESULT` and `TEXT_MESSAGE_CONTENT` preserves `<gap:untrusted>` spotlighting (`CMP-028`).

## 2. Multi-Provider System-1 Decision Models (`ADR-028`)

In late 2026, non-autoregressive **System-1 decision models** standardized around single-pass typed questions (`choice`, `score`, `noul` with calibrated probabilities and zero prose output tokens):
- **Convai Innovations `Laya`** (`421M` ModernBERT-large open weights) & **`@receptron/laya`** (`github.com/receptron/laya` ONNX Runtime API).
- **TypeSafe AI `Jev`** (`POST /v1/systemone`, `model: "jev-latest"`, bearer API auth).
- **Google `Gemini Decision 1`** (`POST /v1beta/decisions`, `model: "gemini-decision-1"`, `x-goog-api-key` auth, up to 1,000 parallel questions over shared input).

In `gap-s1` (`Layer L3`), all three wire codecs implement unified typed evaluation (`Choice`, `Score`, `Noul`) and convert probabilities into raw log-odds (`ln(clamp(p, 1e-6, 1.0))`) so `gap-route` can calibrate temperatures (`T_b`), enforce split-conformal prediction sets (`ADR-008`), guard egress via `EgressGuardHook`, redact API keys (`INV-15`), and preempt timeouts back to `BaselineRouter` (`INV-17`, `RTE-044`). `ARC-002` guarantees none of these System-1 providers ever execute on the typed execution or authorization path.
