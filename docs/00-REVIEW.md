# 00 — Review độc lập session "Thiết Kế Protocol Mới Cho Agent"

> Ngày review: 2026-10-26 · Đối tượng: session Antigravity `ecc62033-9e38-45ee-8736-5c86362ff864` (79 bước, 3 lượt user, 4 artifact, 5 sub-agent nghiên cứu)
> Ngôn ngữ: tài liệu cho người đọc (review, plan tổng quan) viết bằng tiếng Việt; tài liệu **chuẩn tắc cho agent** (`spec/`, `work/`, `docs/AGENT-GUIDE.md`, ADR) viết bằng tiếng Anh để chính xác và dùng được khi open-source.

## 1. Kết luận

**Hướng đi đúng, bản plan/spec cũ thì chưa đủ điều kiện giao cho AI Agent.**

- **Tầm nhìn được xác nhận.** Một lớp giao thức có kiểu (typed graph) hợp nhất tool + data + code/doc context + agent, với *progressive disclosure* thay vì đổ toàn bộ tool schema vào context, là hướng khả thi và đã có tiền lệ công nghiệp (Apollo MCP Server, WunderGraph Cosmo, GraphQL AI WG `__search`, Hasura NDC `foreach` joins). Các mẫu kiến trúc cốt lõi (pipeline `tower` nhiều tầng, snapshot bất biến + telemetry tách rời, trích xuất xác định → LLM có ràng buộc → kiểm chứng ký hiệu, từ chối thay vì bịa tool call) được giữ lại.
- **Nhưng 8 vấn đề mức S0 (blocker)** khiến plan cũ không thể triển khai "từ đầu đến cuối" mà vẫn đạt production-grade: (1) `open-jev` không có trọng số đã huấn luyện, nên "hợp nhất open-jev + Laya" thực chất là dự án huấn luyện model; (2) checkpoint được khuyến nghị (`laya-multilingual`) gần như ngẫu nhiên trên bài toán typed-decisions; (3) System-1 bị đặt trên đường nóng của *mọi* request, trộn lẫn "thực thi truy vấn có kiểu" (xác định) với "khám phá theo ý định" (xác suất); (4) cổng từ chối `max(p)` xung đột với bản sao (replica) của cùng một nhánh; (5) xác minh cạnh `@join` dựa vào LLM/Laya → join sai = dữ liệu sai âm thầm; (6) "tự khám phá từ `$PATH`/registry/MCP server" + chạy `--help` của binary lạ = thực thi/ingest mã không tin cậy, không có mô hình đe dọa; (7) EBNF "normative" thiếu 21 nonterminal và phá vỡ tooling GraphQL; (8) spec không thực thi được bởi agent (không có work package, tiêu chí nghiệm thu, DAG dùng field không tồn tại).
- **Nhiều con số trong plan là sai hoặc không có nguồn** (xem §3 nhóm A): swap `<5 ns` (đo thật: 0,16–23 µs), `load ~1 ns` (đo thật: 15 ns), "15–35 ms CPU" (thực ra là T4 GPU; CPU ~100–140 ms), "90× / \$440K/tháng" (45k token là chi phí *quét file code*, không phải liệt kê 50 MCP tool).
- **Bản sửa** nằm ở `docs/01-DECISIONS.md` (24 ADR), `spec/` (đặc tả chuẩn tắc), `docs/02-PLAN.md` + `work/` (kế hoạch thực thi dạng work package có cổng nghiệm thu định lượng), `docs/03-VERIFICATION.md` (chiến lược kiểm chứng). Bạn chỉ cần xem lại §5 (các quyết định cần xác nhận) — mọi mục đều đã có mặc định khuyến nghị nên agent có thể chạy ngay.

## 2. Phạm vi & phương pháp review

| Đã đọc | Chi tiết |
|---|---|
| Toàn bộ hội thoại | 3 yêu cầu của bạn (bước 0, 18, 69), câu trả lời Q&A (Rust / thay thế cả MCP+A2A / full production), toàn bộ tool call và báo cáo của 5 sub-agent |
| 4 artifact | `graph-agent-protocol-plan.md`, `gap-agent-spec-and-skill.md`, `research_notes.md`, `gap-v2-detailed-review.md` |
| Môi trường | `omni` 0.1.206 (`omni decide`, `omni docs`, schema DAG/skill), `~/.gemini/GEMINI.md` (quy tắc "Bite-Sized Plans", GitButler, Conventional Commits) |

**Cách kiểm chứng (không tin vào artifact cũ):**

| Loại | Việc đã làm |
|---|---|
| Đo thật | Benchmark `arc-swap` 1.9.3 trên máy bạn (`docs/evidence/arcswap-bench/`); script kiểm tra nonterminal chưa định nghĩa trong EBNF |
| Nguồn gốc | Đọc README `NandhaKishorM/laya`, `receptron/laya`, `kyegomez/open-jev`, `aovestdipaperino/laya-rust`, model card `convaiinnovations/laya-multilingual` |
| Hệ sinh thái | crates.io API (phiên bản, license, phụ thuộc: ~70 crate), `omni docs` (schema DAG, skill), tìm kiếm web về MCP `2026-07-28`, A2A v1.0, tool-poisoning/rug-pull |

**Giới hạn của review này (nói thẳng):** chưa chạy thử model Laya (tải ~0,85–1,7 GB) nên mọi con số hiệu năng/độ chính xác của System-1 vẫn là *số của nhà cung cấp*, không phải số đo của GAP; spec biến chúng thành **cổng nghiệm thu bắt buộc đo lại** (milestone M2/M5). Chưa kiểm tra mã nguồn Apollo/Hasura trực tiếp; các mô tả pipeline lấy từ báo cáo sub-agent (có dẫn tên file cụ thể, hợp lý với kiến thức công khai) và được xếp là "đã xác nhận ở mức kiến trúc".

## 3. Phát hiện

Mức độ: **S0** blocker (làm hỏng thiết kế hoặc không thể triển khai) · **S1** nghiêm trọng · **S2** nhỏ. Cột "Sửa" trỏ tới ADR (`docs/01-DECISIONS.md`) và spec.

### A. Tuyên bố sai hoặc không có nguồn

| ID | Mức | Tuyên bố trong plan cũ | Thực tế (bằng chứng) | Sửa |
|---|---|---|---|---|
| F-01 | S1 | `ArcSwap::load ~1 ns`, `store < 5 ns` | Đo thật (arc-swap 1.9.3, `--release`): `load()` **15 ns**; `store()` p50 **164 ns** không reader → **1,7 µs** (8 reader) → **6,5 µs** (64) → **22,8 µs** (256), p99 59 µs; thả snapshot 400 MB trên thread swap mất **28,9 ms**. | ADR-009: SLO đúng; thu hồi snapshot ngoài thread swap; cam kết là *đọc wait-free, không thấy trạng thái nửa vời*, không phải độ trễ swap. |
| F-02 | S0 | Hợp nhất `open-jev` StateCache + Laya, ECE ≤ 0,10, xong Phase 2 (W4–6) | README `kyegomez/open-jev`: *"unofficial research implementation with random weights. It is not the production Jev model"*; việc "publish trained checkpoints" còn nằm trong TODO. Crate `openjev` trên crates.io = "Reserved". Không có trọng số ⇒ phải tự huấn luyện (RLCD, dữ liệu chưng cất). | ADR-004: bỏ khỏi đường găng; chỉ lấy ý tưởng *không cần huấn luyện* (canonical hóa JSON key-sorted). Huấn luyện model riêng là **track R&D R1** có cổng quyết định, không phải việc kỹ thuật. |
| F-03 | S0 | Khuyến nghị `laya-multilingual` 322M cho routing (vì tiếng Việt) | Model card: *"Performs near chance on typed-decisions zero-shot (0.342)"*, macro-ECE **0,387**, *"Ships uncalibrated"*. Chỉ `laya-typed-decisions` đạt **0,766** (tiếng Anh). `laya-rust`: base checkpoint là *"a fast base to specialise, not a zero-shot decision engine"*. | ADR-005: mặc định `laya-typed-decisions` + hiệu chuẩn + (nếu cần) fine-tune; tiếng Việt xử lý ở phía agent (chuẩn hóa intent sang tiếng Anh) hoặc track fine-tune đa ngữ. |
| F-04 | S1 | "15–35 ms CPU / <15 ms Metal" | 32,8 ms của model card là **T4 GPU**. Báo cáo sub-agent: *"~100–140 ms for 3 batched questions on Apple M-series CPU, ~30–40 ms on GPU"*. Cộng thêm embed truy vấn (bi-encoder) bị bỏ sót. | ADR-003: S1 ra khỏi đường nóng; SLO khám phá có S1: p95 ≤ 250 ms CPU / ≤ 80 ms GPU; retrieval-only ≤ 40 ms. |
| F-05 | S1 | "ECE 0,081 → mục tiêu ≤ 0,10 cho GAP" | 0,466→0,081 là trên dữ liệu của tác giả với nhiệt độ đóng gói sẵn; `laya-rust`: *"Refit one temperature per bucket on your own data before trusting the probabilities."* Độ chính xác công bố: MASSIVE-en **0,783** ở 20 lựa chọn ⇒ top-1 ~78% không thể là cổng cứng không có dự phòng. | ADR-007/008: GAP-RouteBench riêng, hiệu chuẩn lại, split-conformal, cổng `ECE ≤ 0,05` đo trên tập giữ lại; S1 chỉ bật mặc định nếu thắng baseline. |
| F-06 | S1 | "45.000 → ~500 token (90×), tiết kiệm \$440K/tháng; 10M req/s" | 45k token trong báo cáo nghiên cứu là *"Specific Query Overhead (file scanning)"* cho tác vụ code retrieval, **không phải** chi phí liệt kê 50 MCP tool. ~500 là mục tiêu thiết kế. Bỏ qua prompt caching (tiền tố tool tĩnh được cache giảm giá mạnh). "10M req/s" là lỗi gõ. | ADR-021: gỡ các con số kinh doanh khỏi spec; thay bằng **GAP-Bench** (đo token/độ chính xác/độ trễ với N ∈ {10, 50, 200, 1000} tool, có/không prompt cache). |
| F-07 | S2 | Dùng `laya-onnx` (crate chính thức) + `ort = "2.0"` | `laya-onnx` **404** trên crates.io. Có `laya` 0.1.1 (bên thứ ba, backend candle 0.9, 17 sao/6 commit), `laya-rust` 0.1.0 (17 lượt tải, không repo). `ort` vẫn **2.0.0-rc.15**. | ADR-005: `System1Backend` là trait; backend `candle` do ta sở hữu (vendor + golden test so với PyTorch), `ort` là feature tùy chọn, pin chính xác `=2.0.0-rc.15`. |
| F-08 | S2 | `Cargo.toml` mẫu | `tonic 0.12.3` phụ thuộc `axum ^0.7` ⇒ trộn với `axum 0.8` sinh **hai bản axum** trong cây phụ thuộc (không chia sẻ được `Router`/service). `prost 0.13`→0.14.4, `candle 0.8`→0.11.0, `surrealdb 2.2`→3.3.4 (license `non-standard`/BSL, mâu thuẫn với "Apache-2.0"), `opentelemetry 0.27`→0.33.1, `petgraph 0.7`→0.8.3, `tokenizers 0.21`→0.23.2, `rmp-serde` không dùng. | ADR-002 + `docs/evidence/crate-versions.md`: bảng phiên bản đã kiểm chứng ngày 2026-10-26; `cargo-deny` chặn license lạ. |
| F-09 | S0 | "EBNF ISO là Normative Spec" | Script kiểm tra: **21 nonterminal được dùng nhưng không định nghĩa** (`Name, SelectionSet, VariableDefs, Directives, Args, FieldDef, SchemaDef, EnumDef, ScalarDef, InterfaceDef, InputObjectDef, DirectiveDef, Description, ImplementsInterfaces, IntValue, StringValue, NodeSelector, EdgeFilter, TemporalSelector, PredicateExpr, BranchRef`). Từ khóa mới `traverse {…}`/`federate {…}` làm tài liệu **không còn hợp lệ GraphQL**: mất tooling, mất độ quen thuộc với LLM. | ADR-001: GAP-QL = *profile* của GraphQL (parse bằng `apollo-compiler`), `traverse`/`search`/`resolve` là field gốc có kiểu; `federate` → Task API. |
| F-10 | S1 | DAG `omni run --dag` với `write_scopes` để tránh xung đột file | `omni docs dag-patterns` + `swarm-engine`: không có `write_scopes`. Field hợp lệ: `id, title, role, depends_on, tools, rules, system_prompt` (+ cấp trên: `project_root, tdd, auto_commit`). | ADR-022: cô lập bằng *một branch/worktree cho mỗi work package* + lint "path ownership" trong CI; DAG sinh tự động từ `work/work-packages.toml` và được `tools/wp.py validate` kiểm tra. |
| F-11 | S2 | Cài skill vào `~/.config/_skills_/` ngay khi duyệt plan | Ghi ra ngoài project, đổi cấu hình toàn cục của máy. | Skill đặt trong repo `.agent/skills/`; cài global là hành động của bạn (§5). |

### B. Lỗ hổng logic thiết kế

| ID | Mức | Vấn đề | Hậu quả | Sửa |
|---|---|---|---|---|
| F-12 | S0 | **System-1 đứng trên đường nóng của mọi request** (pipeline: MasterGraph ↔ S1 ↔ Exec), cả khi client đã gửi truy vấn GAP-QL có kiểu. | Truy vấn có kiểu vốn xác định hoàn toàn bằng schema (như Apollo planner); thêm 100+ ms CPU, thêm nguồn phi tất định, và đặt quyết định *ủy quyền* vào model. | ADR-003: **ba mặt phẳng** — thực thi xác định (không ML); khám phá theo ý định (`search`/`resolve`, có ML); điều khiển (ingestion). Quyền truy cập luôn do policy xác định quyết định. |
| F-13 | S0 | Cổng từ chối dùng `answer_confidence = max(p)` trên phân phối **qua các nhánh**, nhưng nhiều nhánh là bản sao tương đương. | Hai bản sao tốt ⇒ mỗi bản ~0,5 ⇒ dưới ngưỡng `choice:2 → 0,75` ⇒ **từ chối nhầm**; hoặc LB thiên lệch theo xác suất mô hình. | ADR-006: S1 chọn giữa **nhóm năng lực (capability group)**; chọn bản sao trong nhóm là việc của LB (power-of-two-choices) dựa trên telemetry. |
| F-14 | S1 | Định tuyến ở mức **nhánh** với synopsis ≤ 20 token; centroid `[M,384]` không nói model embed nào; thời gian embed truy vấn bị bỏ sót ("<1 ms shortlist"). | API 800 endpoint không thể tóm gọn trong 20 token; thiếu chỉ mục theo thao tác ⇒ agent vẫn phải đọc schema lớn. | ADR-010: chỉ mục theo **thao tác/field/type** (lai dense + BM25, RRF); synopsis chỉ cho tầng L0 `__metaGraph`; model embed được pin trong snapshot; HNSW chỉ khi >20k vector (flat SIMD đủ nhanh). |
| F-15 | S1 | "Conformal Abstention Gate" thực chất là bảng ngưỡng theo bucket trên `max(p)` (đúng như Laya `min_confidence_map`) — **không phải conformal**; không có pipeline dữ liệu hiệu chuẩn; cold-start chỉ "T=1,0 và log cảnh báo". | Không có bảo đảm độ phủ; ngưỡng mặc định của Laya lấy từ workload khác. | ADR-008: split-conformal thật (LAC/APS) trên tập hiệu chuẩn của GAP; ngưỡng bucket chỉ là fallback khi `n_cal` nhỏ. |
| F-16 | S0 | Cạnh ảo `@join` được "xác minh" bằng `noul ≥ 0,85` của Laya (`is_same_entity_key`) + kiểm tra kiểu. Laya chưa từng được kiểm chứng cho schema matching zero-shot. | Một join sai (ví dụ `customer.id` ↔ `invoice.id` cùng kiểu UUID) **không gây lỗi**, chỉ trả dữ liệu sai âm thầm. | ADR-012: xác minh **dựa trên dữ liệu** (độ chứa của sketch giá trị, độ duy nhất khóa RHS, tỷ lệ null, ước lượng cardinality) + canary + phê duyệt người cho join xuyên miền tin cậy; điểm S1 chỉ là tín hiệu phụ. |
| F-17 | S0 | Khám phá tự động từ "MCP server, CLI trong `$PATH`, registry" và trích xuất bằng `--help`; không có mô hình đe dọa. | Chạy binary lạ = RCE; mô tả tool độc (tool poisoning), "rug pull" (đổi mô tả sau khi duyệt), shadowing giữa các server. Plan có `spec_blake3` nhưng không có quy trình khi hash đổi. | ADR-013/014: bậc tin cậy T0/T1/T2, **propose-not-activate**, cách ly (quarantine), ghim hash mô tả + tái duyệt khi lệch, sandbox cho CLI, allow-list registry, SSRF guard. |
| F-18 | S1 | Telemetry: EWMA `load→tính→store` bị mất cập nhật khi đồng thời; trọng số theo *mẫu* (không phân rã theo thời gian) ⇒ nhánh nhàn rỗi giữ số liệu cũ; `in_flight` rò rỉ khi hủy/panic; chọn `argmax U` ⇒ "bầy đàn" dồn tải; công thức `U` trộn đơn vị với λ tùy ý; breaker chỉ là một trường số, không có máy trạng thái. | Dao động tải, breaker kẹt, rò đếm. | ADR-011: EWMA phân rã theo thời gian bằng CAS, RAII guard cho in-flight, P2C + hysteresis, breaker Closed/Open/HalfOpen có probe, ngân sách retry. |
| F-19 | S1 | Rollback tự động "lỗi >5% trong 30 s" không phân biệt nguyên nhân; khóa plan-cache chỉ theo hash truy vấn. | Rollback nhầm do sự cố hạ tầng không liên quan; plan phụ thuộc policy/tenant bị dùng chéo ⇒ rò quyền. | ADR-009: so sánh với baseline thế hệ trước, ngưỡng mẫu tối thiểu, chỉ rollback một lần rồi đánh dấu thế hệ xấu; khóa cache = `(tenant, policy_ver, snapshot_branch_set, query_hash)`. |
| F-20 | S1 | Remote join thiếu ngữ nghĩa: giới hạn batch, null/trùng khóa, bùng nổ nhiều-nhiều, lỗi từng phần, phân trang xuyên nhánh, nhất quán xuyên nhánh, retry/storm. | Hành vi không xác định ⇒ mỗi agent triển khai một kiểu. | `spec/06-runtime.md` §Joins (quy tắc chuẩn tắc + test). |
| F-21 | S1 | "Thay thế A2A" nhưng chỉ có 4 RPC; mất ngữ nghĩa Task dài hạn, hủy, `input-required`, push. GAP-to-GAP không được định nghĩa (vòng lặp). | Không tương thích A2A v1.0 (đã ổn định 03/2026, thuộc AAIF 08/2026). | ADR-016: Task API kiểu A2A (8 trạng thái), durable; `hop`/`trace` chống vòng khi peer. |
| F-22 | S1 | Mutation chỉ có `idempotency_key`; không phân loại tác dụng phụ, không dry-run, không phê duyệt người, không bù trừ. Gợi ý của MCP (`readOnlyHint`…) bị coi như thật. | Tác dụng phụ không thể đảo ngược do agent bị prompt-injection. | ADR-015: `@sideEffect(level)`, mặc định từ chối, dry-run, `input-required` cho IRREVERSIBLE, saga tùy chọn, hint của server = *không tin cậy*. |
| F-23 | S1 | Không có **mặt tiền MCP/A2A phía vào** (inbound); chỉ có adapter phía ra. | Host hiện có (Claude Code, Antigravity, Cursor) không dùng được GAP ngày 1 ⇒ không có phân phối, không đo được giá trị. | ADR-017: GAP expose 5 meta-tool tĩnh (`gap_search/describe/query/mutate/task`, trong đó `gap_query` chỉ-đọc để host tự duyệt) qua MCP `2026-07-28` (stateless) + A2A v1.0 ngay từ M1. |
| F-24 | S1 | Đa tenant bị hoãn, nhưng khóa cache/telemetry/audit/policy đều cần chiều tenant. | Sửa sau rất đắt, dễ rò dữ liệu chéo tenant. | ADR-018: `tenant_id` trong mọi khóa từ M1 (triển khai đơn tenant). |
| F-25 | S2 | Tarjan SCC + `max_depth ≤ 16` bị dùng như quy tắc chặn chu trình schema; HTTP `504` cho InsufficientEvidence; mã lỗi số. | Chu trình trong schema đồ thị là hợp lệ; 504 là Gateway Timeout. | `spec/02-gap-ql.md`: giới hạn độ sâu/chi phí lúc chạy; mã lỗi chuỗi ổn định; lỗi nghiệp vụ trả 200 + `errors[]` theo GraphQL-over-HTTP (ADR-023). |
| F-26 | S2 | Phạm vi phình: QUIC, plugin tĩnh + Rhai + coprocessor, SurrealDB bắt buộc, 14 crate, `rmp-serde`. | Chậm, nhiều bề mặt tấn công. | ADR-019/020: hoãn QUIC; Rhai sandbox sau 1.0-rc; `ContextStore` là trait, mặc định SQLite; SurrealDB là feature tùy chọn (license BSL). |
| F-27 | S1 | Phân phối model: 0,84–1,7 GB trọng số nhưng kỳ vọng "1 binary"; nguồn dữ liệu huấn luyện Laya *không được nêu*; SurrealDB BSL. | Rủi ro pháp lý/triển khai. | ADR-005/020: `gap models pull` với checksum pin, chế độ offline, `cargo-deny`, mục rà soát pháp lý trong M5. |

### C. Lỗ hổng spec & khả năng thực thi bởi agent

| ID | Mức | Vấn đề | Sửa |
|---|---|---|---|
| F-28 | S0 | Kế hoạch theo "tuần/phase", không có work package, tiêu chí nghiệm thu từng bước, điều kiện dừng, định nghĩa "xong"; 3 Open Questions chưa được trả lời nhưng chặn bước khởi động; thư mục chưa `git init`; vi phạm quy tắc "Bite-Sized Plans" trong `GEMINI.md` của bạn. | `docs/02-PLAN.md`, `work/work-packages.toml` (85 WP có phụ thuộc, phạm vi đường dẫn, test nghiệm thu; validator tự động), `docs/AGENT-GUIDE.md` (giao thức mở rộng WP thành bước 2–5 phút). |
| F-29 | S1 | Thiếu bộ **conformance suite độc lập ngôn ngữ** (điều kiện để thành "protocol"), thiếu RouteBench/GAP-Bench, chaos/soak, danh sách fuzz target; `loom` cập nhật lần cuối 2024-04 và không mô hình hóa nội bộ `arc-swap`. | `docs/03-VERIFICATION.md`: conformance (fixture JSON/GraphQL), property test, fuzz (parser, NGC, extractor), `shuttle` cho máy trạng thái tự viết, chaos, soak 72h. |
| F-30 | S2 | "v2" là số vòng lặp của plan, bị dùng như số phiên bản giao thức; trùng tên (`gap`, `gap-protocol` đã bị chiếm trên crates.io; GAP còn là viết tắt Bluetooth/hệ đại số). | Phiên bản spec `0.1.0-draft`; chuỗi wire `gap/0.1`; quyết định tên là D-001 (§5), ADR-024. |

### D. Đánh giá lại (thay cho scorecard 8,2/10 trước đó)

Scorecard cũ cho "Tính đúng đắn kiến trúc 9,0/10" trong khi các tuyên bố hiệu năng cốt lõi không được kiểm chứng; điểm đó bị thổi phồng.

| Chiều | Plan cũ (tự chấm) | Review này (trước sửa) | Mục tiêu sau khi áp dụng bộ tài liệu mới |
|---|---|---|---|
| Hướng kiến trúc | 9,0 | **8,0** | 8,5 |
| Tính trung thực của bằng chứng | — | **4,5** | 9 (mọi số liệu có nguồn hoặc là cổng đo) |
| Độ đầy đủ spec | 8,0 | **4,0** | 8,5 |
| Khả năng thực thi bởi agent | 7,5 | **3,0** | 8,5 |
| Sẵn sàng production | 7,0 | **3,0** | 8 tại 1.0-rc, đo bằng cổng ở `docs/03-VERIFICATION.md` §9 |

## 4. Những gì được xác nhận và giữ nguyên

| # | Quyết định cũ | Lý do giữ |
|---|---|---|
| K-1 | Rust cho lõi; Apache-2.0; thay thế MCP+A2A là *đích dài hạn* (lựa chọn của bạn ở bước 14) | Giữ. Nhưng đường đi là "superset + mặt tiền" trước (ADR-017). |
| K-2 | Pipeline `tower` nhiều tầng + shadow build + prewarm plan cache + tái dùng connection pool | Mẫu Apollo Router đã chứng minh. |
| K-3 | Snapshot bất biến (`ArcSwap`) tách khỏi bảng telemetry atomic | Đúng và tinh tế; sửa chi tiết (F-01, F-18). |
| K-4 | Batched `foreach` remote join + remote predicate (kiểu Hasura NDC) | Triệt N+1 là giá trị thật; bổ sung ngữ nghĩa (F-20). |
| K-5 | Trích xuất xác định → LLM có ràng buộc ngữ pháp → cổng ký hiệu | Đúng mẫu; thêm cổng dữ liệu (F-16) và bảo mật (F-17). |
| K-6 | "Từ chối thay vì bịa tool call" | Giữ làm nguyên tắc; sửa cơ chế (F-13, F-15). |
| K-7 | Chi tiết Laya đúng: lọc `[MASK]`, kẹp nhiệt độ `[0,5; 5,0]`, dùng `answer_confidence` thay `act_probability`, giới hạn ~20 lựa chọn, shortlist/tournament | Khớp README `receptron/laya`/`laya-rust`/model card. |
| K-8 | Thang đo "progressive disclosure" `__metaGraph` → `__search` → `__type` → execute | Khớp đề xuất GraphQL AI WG; thành tầng L0–L3 trong spec. |
| K-9 | W3C PROV-O cho provenance; OTel; OAuth 2.1 + RFC 8693 | Giữ; bổ sung RFC 9728/8707 theo MCP authorization. |

## 5. Quyết định cần bạn xác nhận (mỗi mục đã có mặc định — agent KHÔNG bị chặn)

| ID | Câu hỏi | Mặc định đã chọn | Ảnh hưởng nếu đổi |
|---|---|---|---|
| D-001 | Tên giao thức & binary | Tên làm việc **GAP**, binary `gap`; tiền tố crate `gap-` (lưu ý `gap-protocol` và `gap` đã bị chiếm trên crates.io) | Đổi tên là thao tác cơ học nếu làm trước khi kết thúc M0; sau đó đắt. |
| D-002 | System-1 mặc định | `laya-typed-decisions` (EN) qua backend `candle`; chỉ bật mặc định nếu thắng baseline retrieval tại cổng M5 | Nếu chọn multilingual: cần track fine-tune đa ngữ (+4–6 tuần). |
| D-003 | Kho lưu context graph | SQLite (rusqlite) mặc định; SurrealDB 3.x là feature tùy chọn | SurrealDB mặc định: license BSL, build nặng; đổi bằng trait `ContextStore`. |
| D-004 | Phạm vi lần chạy đầu tiên | **M0 + M1** (walking skeleton, có MCP facade chạy được với Claude Code/Antigravity) | Có thể gộp M2 nếu muốn thấy số đo token sớm. |
| D-005 | Cài skill `gap-protocol-architect` vào `~/.config/_skills_/` | **Không** tự cài; bản trong repo `.agent/skills/` | Bạn chạy `omni build-index` nếu muốn global. |
| D-006 | Cho phép Synthesizer gọi LLM cloud | Mặc định **chỉ provider cục bộ/được cấu hình**; không gửi nội dung nguồn lên cloud nếu chưa bật `allow_cloud_enrichment` | Ảnh hưởng quyền riêng tư dữ liệu nguồn. |

## 6. Phụ lục — số liệu và nguồn

- Benchmark `arc-swap`: `docs/evidence/arcswap-bench/` (mã + kết quả).
- Phiên bản crate/dependency đã kiểm chứng: `docs/evidence/crate-versions.md`.
- Nguồn chính (đã truy cập 2026-10-26): `github.com/NandhaKishorM/laya`, `github.com/receptron/laya`, `github.com/aovestdipaperino/laya-rust`, `github.com/kyegomez/open-jev`, `huggingface.co/convaiinnovations/laya-multilingual`, `modelcontextprotocol.io/specification/2026-07-28/changelog`, `linuxfoundation.org/press/a2a-protocol-surpasses-150-organizations…`, `labs.cloudsecurityalliance.org` (MCP Tool Poisoning and Rug Pull Attacks, 2026-05).
