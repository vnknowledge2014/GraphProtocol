# GAP — Graph Agent Protocol

**English summary.** GAP is a proposed protocol layer between AI agents and everything they need (APIs, databases, MCP servers, other agents, code and documentation graphs). A client writes ordinary GraphQL (a strict *profile*, ADR-001); discovery is progressive (`__metaGraph` → `__search` → `__type` → execute) so context cost does not grow with the number of tools; cross-backend joins are batched (no N+1); authorization is deterministic (Cedar); long-running work uses A2A-compatible Tasks; new sources are *proposed, verified and hot-swapped* into an immutable snapshot with rollback. A small calibrated decision model (System-1) is an **optional, gate-measured** discovery component — the system is fully functional without it. Existing MCP hosts use GAP today through a five-tool MCP facade.

Status: **specification + execution plan complete; implementation starts at `WP-M0-01`.** Nothing here claims measured performance yet — numbers are targets/gates until `bench/results/` exists (ADR-021).

---

## Tài liệu (tiếng Việt cho người đọc; tiếng Anh cho phần chuẩn tắc)
| Cần gì | Mở file |
|---|---|
| Kết luận review session Antigravity, lỗi đã sửa, bằng chứng | [`docs/00-REVIEW.md`](docs/00-REVIEW.md) |
| 24 quyết định kiến trúc (ADR) | [`docs/01-DECISIONS.md`](docs/01-DECISIONS.md) |
| Kế hoạch: milestone, làn song song, lịch, rủi ro, cổng | [`docs/02-PLAN.md`](docs/02-PLAN.md) |
| Chiến lược kiểm chứng + checklist phát hành 1.0-RC | [`docs/03-VERIFICATION.md`](docs/03-VERIFICATION.md) |
| Cách AI agent làm việc trong repo này | [`docs/AGENT-GUIDE.md`](docs/AGENT-GUIDE.md), [`AGENTS.md`](AGENTS.md) |
| Đặc tả chuẩn tắc | [`spec/`](spec/) — 00 quy ước · 01 kiến trúc · 02 GAP-QL · 03 NGC · 04 routing/calibration/balancing · 05 ingestion · 06 runtime · 07 security · 08 operations · 09 compat (MCP/A2A/connectors) |
| Hợp đồng nhị phân | [`proto/gap/ngc/v1/ngc.proto`](proto/gap/ngc/v1/ngc.proto), [`proto/gap/gateway/v1/gateway.proto`](proto/gap/gateway/v1/gateway.proto) |
| 85 work package + DAG cho omni | [`work/work-packages.toml`](work/work-packages.toml), [`work/dag/`](work/dag/) |
| Số liệu thật đã đo / phiên bản đã kiểm chứng | [`docs/evidence/`](docs/evidence/) |

## Bắt đầu
```bash
python3 tools/wp.py validate && python3 tools/wp.py ready   # 85 WP, 281 agent-day, đường găng 65,5
omni run --estimate-dag work/dag/m0.json                    # xem trước chi phí M0
```
Quyết định cần bạn xác nhận (đều có mặc định, không chặn agent): tên giao thức, checkpoint System-1, kho context, phạm vi lần chạy đầu, cài skill global, cho phép LLM cloud — xem `docs/00-REVIEW.md` §5.

License: Apache-2.0 (planned; see WP-M0-01).
