# 02 — Kế hoạch triển khai GAP (từ commit đầu tiên đến 1.0)

> Thay thế `graph-agent-protocol-plan.md` (Antigravity). Đơn vị công việc là **Work Package (WP)** trong `work/work-packages.toml` (85 WP, đã qua `python3 tools/wp.py validate`: đồ thị phụ thuộc không chu trình, tham chiếu spec hợp lệ, WP chạy song song có phạm vi sở hữu rời nhau). Chuẩn tắc kỹ thuật ở `spec/`, lý do ở `docs/01-DECISIONS.md`, kiểm chứng ở `docs/03-VERIFICATION.md`, cách agent làm việc ở `docs/AGENT-GUIDE.md`.

## 1. Phạm vi 1.0 và định nghĩa "xong"
**GAP 1.0** = giao thức + server tham chiếu bằng Rust, trong đó một AI agent có thể: (1) khám phá có kiểm soát token (`__metaGraph` → `__search` → `__type` → thực thi); (2) chạy truy vấn/đột biến xuyên nhiều backend (OpenAPI/REST, SQL, gRPC, GraphQL, MCP, A2A, CLI sandbox, code/doc graph) với join `foreach` không N+1; (3) duyệt đồ thị code/tài liệu (`gap.traverse`); (4) giao việc dài hạn qua Task API tương thích A2A v1.0; (5) dùng ngay từ host MCP hiện có (mặt tiền MCP 5 tool tĩnh); (6) thích ứng nóng với nguồn mới qua quy trình *đề xuất → xác minh → duyệt → hoán đổi snapshot không downtime*; tất cả dưới ủy quyền xác định (Cedar), cách ly tenant, kiểm toán băm chuỗi.
**"Xong" được định nghĩa bằng `docs/03-VERIFICATION.md` §9** (21 cổng G-1…G-21, mỗi cổng có bằng chứng). Không dùng mô tả định tính.
System-1 (Laya) là **thành phần tùy chọn có cổng đo** (G-S1): sản phẩm phải đầy đủ chức năng khi S1 tắt (INV-17).

## 2. Nguyên tắc thực thi
1. **Lát cắt dọc, đi bộ trước khi chạy:** M1 cho một đường thẳng end-to-end (host MCP → GAP → 2 backend thật) rồi mới mở rộng chiều sâu.
2. **Đo trước, tuyên bố sau:** mọi con số là *Target → Gate/SLO* (spec/00 §3). Số liệu GAP-Bench/RouteBench được công bố cả khi bất lợi.
3. **Cổng định lượng cuối mỗi milestone** do WP role `reviewer` ghi vào `docs/gates/Mx.md`; cổng trượt thì dừng milestone, thêm WP sửa, không hạ ngưỡng.
4. **WP ≤ cỡ L** (≤ 5 agent-day); cỡ XL bị cấm — tách trước khi bắt đầu. Mỗi WP mở rộng thành bước 2–5 phút trước khi code (quy tắc của bạn).
5. **Bảo mật không để cuối:** tenant, trust tier, no-token-passthrough, sandbox fail-closed được thiết kế từ M0–M1; M6 là *hoàn thiện và kiểm chứng đối kháng*, không phải "thêm bảo mật".
6. **Cô lập song song:** một WP = một branch; sở hữu đường dẫn rời nhau; scaffolding module có sẵn (AGENT-GUIDE §6).

## 3. Milestones
Ước lượng là *agent-day* (S=0,5, M=2, L=5), cộng theo WP; "đường găng" là chuỗi phụ thuộc dài nhất trong milestone (từ `wp.py critical-path`). Đây là con số lập kế hoạch, không phải cam kết.

| M | Mục tiêu | WP | Tổng / đường găng | Cổng thoát (tóm tắt; chi tiết trong WP gate) |
|---|---|---|---|---|
| **M0 Foundations** | Repo, workspace + dependency đã kiểm chứng, CI, công cụ (`wp`, `check_layers`, `spec_trace`), pipeline proto, `gap-core`, runner conformance + testkit, ghim spec ngoài (MCP/A2A/GraphQL) | M0-01…10 | 15,5 / 7,5 | `just verify` xanh trên macOS+Linux; `cargo tree -d` chỉ một `axum/hyper/prost/tower`; descriptor golden; `third_party/MANIFEST.toml` |
| **M1 Walking skeleton** | Đường typed end-to-end: `gap-ql`, NGC SDK, snapshot tĩnh, planner 1 nhánh, executor, HTTP, **mặt tiền MCP 5 tool**, connector OpenAPI + MCP, server/CLI, ví dụ chạy được | M1-01…13 | 53 / 34 | **G-F1** `tools/list` ≤ 1.200 token với N=5…1.000 nhánh; Core conformance; INV-01/02/03/12; plan tất định; real-host smoke |
| **M2 Context graph + Discovery + Đo lường** | `ContextStore` (SQLite), indexer code/doc (tree-sitter), `gap.traverse`, chỉ mục lai dense+BM25, `__search`/`gap.resolve` baseline + hiệu chuẩn + conformal, **RouteBench v1**, **GAP-Bench v1**, nhóm năng lực | M2-01…09 | 34,5 / 15,5 | **G-C1** (đồ thị code ≤ 15% token so với quét file, độ chính xác ≥), recall@10 ≥ 0,95, `__search` p95 ≤ 40 ms, độ phủ conformal trên baseline, bảng GAP-Bench nền |
| **M3 Federation execution** | `gap-balance` (EWMA/breaker/P2C/retry budget), join `foreach` + remote predicate, lỗi từng phần, deadline/cancel, shedding, connector SQL/gRPC/GraphQL, chaos | M3-01…09 | 33 / 14 | INV-18 (không N+1), chaos subset, mô phỏng không "bầy đàn", 10k rps 5 phút, cold start ≤ 2 s |
| **M4 Evolution** | CISG + extractor, Enricher (Null + LLM ràng buộc), cổng G1–G9, xác minh join theo dữ liệu, quy trình Proposal, Publisher (prewarm/swap/reclaim/rollback), drift, incremental snapshot | M4-01…10 | 39,5 / 22,5 | **G-E1** (bộ vàng), swap stress p99 ≤ 250 µs & 0 request lỗi/đứt, adversarial join, ma trận drift, fuzz extractor |
| **M5 Intelligence (S1)** | Manifest/provenance Laya, backend candle + golden parity, tích hợp S1 an toàn, hiệu chuẩn thật, feedback/online ECE, backend phụ (sidecar/ort), debias, **đánh giá & quyết định G-S1**, (tùy chọn) fine-tune | M5-01…09 | 24 / 18 | **G-S1**: bật S1 chỉ khi thắng baseline ≥ 3 điểm, ECE ≤ 0,05, độ phủ ≥ 1−α−0,02, đúng SLO; ngược lại baseline mặc định + kích hoạt fine-tune/R1 |
| **M6 Tasks, A2A, Mutations, Security** | Authn (OAuth 2.1/JWT/mTLS/token exchange), Task API bền vững, A2A v1.0, đột biến có duyệt + idempotency + saga, Cedar, cách ly tenant, vault/redaction, EgressGuard, sandbox + trust tier + chữ ký manifest, linter injection + Rule of Two, audit hash-chain, quota | M6-01…13 | 42,5 / 17 | SEC §15 đủ 17/17, phục hồi sau crash, luồng duyệt E2E, `audit verify`, fixture A2A, `cargo deny/audit` sạch |
| **M7 Productization → 1.0-RC** | CLI hoàn chỉnh, SDK TS/Python mỏng, docs site có test, dashboard/alert/runbook, công bố benchmark, fuzz 24 h, soak 72 h, upgrade/restore, đóng gói ký, (tùy) columnar, rà soát RC | M7-01…11 | 37 / 9 | Checklist `03-VERIFICATION.md` §9 (G-1…G-21) |
| **R1 (điều kiện)** | Memo go/no-go huấn luyện System-1 riêng — chỉ khi G-S1 trượt sau fine-tune | R1-01 | 2 | – |

Tổng: **85 WP, 281 agent-day, đường găng toàn dự án 65,5 agent-day (17 WP)**, song song hóa tối đa lý thuyết ≈ 4,3×.

## 4. Làn song song và điểm đồng bộ
```
Lane A  Exec core       M0-08 → M1-01 → M1-05 → M1-06 → M1-07 → M1-08 ─┬→ M3-02/03/04/07 → M4-08 → M6-02/04
Lane B  Connectors      M0-06 → M1-03 → M1-04, M1-09/10/11 → M3-05/06 → M2-02 (code graph)
Lane C  Discovery/ML    M2-04 → M2-05 → M2-06/07/08 → M5-01…09           (độc lập với Lane A sau M1-05)
Lane D  Evolution       M4-01 → M4-02/03/04/05 → M4-06/07/09              (cần M1-05, M3-02, M3-05)
Lane E  Security/Tasks  M6-01 → M6-05/07/08/11/12 → M6-09/10/03/04
Lane F  Quality/Ops     M0-04/05/07/09/10 → M3-08 → M7-*
```
Điểm đồng bộ bắt buộc (không WP nào sau điểm này được bắt đầu khi cổng chưa xanh): **M0 gate** (sau `WP-M0-02…10`) → **G-F1/M1 gate** (`WP-M1-13`) → **M2 gate** → **M3 gate** → **M4 gate**, **M5 gate** (`WP-M5-08`), **M6 gate** (`WP-M6-13`) → **RC** (`WP-M7-10`). M3-01 (`gap-balance`), M2-01/04 có thể bắt đầu ngay sau M0 song song với M1.

## 5. Lịch dự kiến (giả định, để lập kế hoạch)
- Công việc thuần: 281 agent-day. Giả định 35% overhead cho review, vòng sửa sau cổng, hợp nhất song song.
- 4 agent đồng thời: ≈ max(65,5; 281/4 = 70) × 1,35 ≈ **95 ngày làm việc ≈ 19 tuần**; 6 agent: bị đường găng chặn, ≈ **88 ngày ≈ 18 tuần**. Thêm agent trên 6 gần như không rút ngắn lịch — hãy đầu tư vào cắt đường găng (ví dụ M1-05→M1-12).
- Mốc: M0 ≈ tuần 1; M1 ≈ tuần 5; M2 ≈ tuần 8; M3 ≈ tuần 10 (song song M2); M4 ≈ tuần 14; M5 ≈ tuần 14 (song song); M6 ≈ tuần 17; RC ≈ tuần 19–20. (Soak 72 h, fuzz 24 h/target chạy nền cuối M6–M7.)

## 6. Rủi ro chính và phương án dự phòng
| ID | Rủi ro | Dấu hiệu / trigger | Dự phòng | Chủ sở hữu WP |
|---|---|---|---|---|
| R-01 | S1 không thắng baseline hoặc ECE > 0,05 | gate G-S1 trượt | giữ `routing.model="baseline"`; fine-tune (M5-09); R1 memo | M5-08 |
| R-02 | `rmcp` không tuân thủ MCP 2026-07-28 | test stateless/_meta trượt | client/server JSON-RPC mỏng tự viết (ADR addendum) | M1-09/10 |
| R-03 | candle chậm/sai lệch so PyTorch | golden parity trượt; p95 vượt SLO | backend `ort` (feature) hoặc `HttpSidecar` | M5-02/06 |
| R-04 | `apollo-compiler` thiếu tính năng/đổi API (2.0 beta) | thiếu `@oneOf` hoặc validate sai | ghim 1.33.x; bọc sau trait; vá/đóng góp upstream | M1-01 |
| R-05 | SQLite không đủ cho đồ thị lớn/HA | vượt trần ING-064 | backend Postgres/Surreal qua trait (M7-11 / hậu 1.0) | M2-01 |
| R-06 | Enricher LLM kém | G-E1 trượt | Null enricher + duyệt người; mô hình tốt hơn qua provider trừu tượng | M4-04 |
| R-07 | Connector không cho lấy mẫu để xác minh join | MCP/REST không có Sketch | join giữ PROPOSED; duyệt người; sketch qua truy vấn đọc có ngân sách | M4-06 |
| R-08 | Tích hợp muộn do phạm vi lớn | gate M1 trễ > 2 tuần | cắt phạm vi M1 (bỏ openapi hoặc mcp outbound), không bỏ MCP facade | M1-13 |
| R-09 | Agent hiểu spec lệch nhau | `spec_trace` báo thiếu test; review phát hiện | quy trình deviation (GUIDE §9), reviewer gates, ADR | tất cả |
| R-10 | `sandbox-exec` (seatbelt) trên macOS bị Apple đánh dấu deprecated | doctor báo không khả dụng | container (Docker/Colima) cho T2 trên macOS; fail-closed | M6-09 |
| R-11 | Spec upstream (MCP/A2A) đổi | CI so checksum `third_party/` | adapter cô lập, cửa sổ tương thích, vendor bản ghim | M0-03, M6-03 |
| R-12 | ML trên CPU quá nặng (RAM ~2 GB) | RSS > ngân sách | S1 tùy chọn; f16/int8; tắt S1 | M5-08 |
| R-13 | Chi phí LLM cho enrichment/bench | ngân sách vượt | provider cục bộ, cache, bộ smoke nhỏ | M2-06/07, M4-04 |
| R-14 | Pháp lý: nguồn dữ liệu huấn luyện Laya *không được nêu* | memo M5-01 kết luận rủi ro | fine-tune trên dữ liệu của ta; hoặc không phân phối trọng số | M5-01/10 |
| R-15 | Hook/môi trường agent cản trở (ví dụ cổng "fact-forcing" của ECC) | agent lặp bị chặn | ghi vào AGENT-GUIDE; chạy với cấu hình hook đã duyệt, không vô hiệu hóa bảo vệ | M0-10 |

## 7. Cổng nghiệm thu tổng hợp
| Cổng | Chỉ số | Ngưỡng | WP |
|---|---|---|---|
| G-F1 | token của `tools/list` MCP facade | ≤ 1.200 (N=5…1.000) | M1-10, M1-13 |
| G-C1 | token để trả lời câu hỏi đồ thị code so với quét file | ≤ 15%, độ chính xác ≥ | M2-09 |
| G-R | recall@10 chỉ mục lai | ≥ 0,95 | M2-04, M2-09 |
| G-N+1 | số lời gọi RHS mỗi join | ≤ ⌈khóa duy nhất/chunk⌉ | M3-03, M3-09 |
| G-SW | swap p99 @256 reader; lỗi/đứt trong 100 swap | ≤ 250 µs; 0 | M4-08, M4-10 |
| G-E1 | độ chính xác join đề xuất / bộ VERIFIED / tên thao tác bịa | ≥ 0,9@recall ≥ 0,6 / ≥ 0,98 / 0 | M4-04 |
| G-S1 | S1 vs baseline | ≥ +3 điểm; ECE ≤ 0,05; coverage ≥ 1−α−0,02 | M5-08 |
| G-SEC | ma trận bảo mật | 17/17 | M6-13 |
| G-RC | checklist phát hành | G-1…G-21 | M7-10 |

## 8. Phạm vi hoãn (ADR-019) và điều kiện xem lại
| Hạng mục | Lý do hoãn | Xem lại khi |
|---|---|---|
| Transport QUIC | không có người dùng/đo lường cần thiết; tăng bề mặt tấn công | có yêu cầu mobile/edge có số liệu |
| Rhai / coprocessor plugin | bề mặt tấn công; Rust `tower::Layer` đủ cho 1.0 | có ≥ 3 yêu cầu extension ngoài cây mã |
| Chế độ TOURNAMENT của federation | S1 chưa được kiểm chứng | sau G-S1 pass |
| `@rateLimit` mức field | giới hạn theo principal/tenant/branch đã có | có nhu cầu SLA theo field |
| Nhân bản đa vùng / HA active-active | cần backend Postgres cho Task/Audit | sau 1.0 (OPS-003) |
| Huấn luyện System-1 riêng (R1) | chỉ khi fine-tune Laya thất bại | memo R1-01 = go |
| Dịch tự động intent đa ngữ | Laya multilingual gần ngẫu nhiên (F-03) | fine-tune đa ngữ đạt gate |
| SDK sinh mã nâng cao, MySQL | ưu tiên SDK mỏng, Postgres/SQLite | phản hồi người dùng |
| Truy vấn khoảng thời gian (`@between`, duyệt lịch sử SUPERSEDES) | 1.0 chỉ cần `asOf`; chưa có nhu cầu đo được (ADR-025) | có ≥ 3 yêu cầu phân tích thay đổi theo khoảng thời gian |
| System-1 tournament nhiều lượt | RTE-030 shortlist một lượt giữ hiệu lực hiệu chỉnh conformal (ADR-025) | sau G-S1 pass và RouteBench cho thấy > 15 nhóm hòa thường xuyên |
| Chuyển socket lắng nghe khi nâng cấp binary đơn node (`LISTEN_FDS`) | zero-downtime đã có qua active/standby, OPS-003 (ADR-025) | có yêu cầu SLA cho triển khai single-node |

## 9. Cách chạy lần đầu (khuyến nghị D-004: M0 + M1)
```bash
cd /Users/mike/Documents/GraphProtocol
python3 tools/wp.py validate && python3 tools/wp.py ready          # chọn WP sẵn sàng
omni run --estimate-dag work/dag/m0.json                            # xem trước chi phí (không thực thi)
omni run --dag work/dag/m0.json                                     # chạy M0 (WP nhỏ phù hợp swarm)
# M1 có WP cỡ L: để agent chủ (Antigravity / Claude Code) điều khiển theo AGENT-GUIDE §3–§4,
# dùng omni cho các WP S/M song song: omni run --dag work/dag/m1.json
```
Prompt mở đầu cho bất kỳ agent nào: *"Đọc `AGENTS.md` và `docs/AGENT-GUIDE.md`. Chạy `python3 tools/wp.py ready`, nhận một WP, mở rộng thành bước 2–5 phút trong `work/steps/<WP>.md`, làm theo TDD, chạy lệnh nghiệm thu, ghi handoff, và dừng nếu gặp mục trong §11 (Stop-and-ask)."*

## 10. Quyết định đang chờ bạn (đã có mặc định; chi tiết ở `docs/00-REVIEW.md` §5)
D-001 tên · D-002 checkpoint S1 · D-003 kho context · D-004 phạm vi lần chạy đầu · D-005 cài skill global · D-006 cho phép LLM cloud khi enrichment.
