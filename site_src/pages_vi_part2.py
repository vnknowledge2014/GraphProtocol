"""Vietnamese documentation pages 8..13 (Tích hợp Agent, HTTP/3 + S1, Bảo mật, Lab, CLI/Ops, Tra cứu)."""
from __future__ import annotations

from .diagrams import svg_facade_flow
from .nav import link_to_asset, link_to_page
from .render import callout, code_block, data_table, esc


def _ctx(slug: str):
    u = lambda target: link_to_page(slug, target, "vi")
    a = lambda asset: link_to_asset(slug, "vi", asset)
    return u, a


def build_vi_part2() -> dict[str, dict]:
    pages: dict[str, dict] = {}

    # 8. FACADES AND HOSTS
    u, _ = _ctx("facades-and-hosts")
    pages["facades-and-hosts"] = {
        "lead": (
            "Một tiến trình máy chủ GAP duy nhất phục vụ đồng thời 3 chuẩn giao tiếp AI mở — MCP 2026-07-28, Agent2Agent (A2A v1.0) và AG-UI v1.0 SSE — giúp các client hiện hữu dùng ngay không cần viết lại giao thức."
        ),
        "learn_bullets": [
            "Cách 5 công cụ MCP tĩnh ánh xạ xuống quy trình khám phá lũy tiến và đột biến được kiểm duyệt.",
            "Xuất Agent Card chuẩn A2A v1.0 với lưu trữ SQLite bền vững và chống vòng lặp hop (`INV-20`).",
            "Phát luồng sự kiện AG-UI `v1.0` (`/ag-ui` SSE) kèm tạm dừng xin phê duyệt con người.",
        ],
        "sections": [
            {
                "id": "three-axis-flow",
                "h2": "Kiến trúc Tương tác 3 Trục",
                "subheadings": [],
                "html": svg_facade_flow("vi")
                + """<p>Hãy hình dung các facade của GAP như tầng phiên dịch đặt trước một động cơ lõi duy nhất: AI host dọc dùng MCP, mạng tác vụ ngang hàng dùng A2A và giao diện web tương tác dùng AG-UI — tất cả dùng chung bản snapshot bất biến và luật kiểm soát Cedar.</p>""",
            },
            {
                "id": "mcp-facade",
                "h2": "Facade Công cụ Dọc: MCP 2026-07-28 (`651` Tokens)",
                "subheadings": [],
                "html": data_table(
                    "5 meta-tool MCP tĩnh trong adapters/gap-mcp",
                    ["Tên Tool", "Chức năng", "Cơ chế Bảo vệ / Chính sách"],
                    [
                        ["<code>gap_search</code>", "Tìm kiếm lai BM25 + HNSW xuyên suốt các nhánh (<code>L1</code>).", "Lọc sẵn theo visibility bitset từ Cedar (<code>INV-05</code>)."],
                        ["<code>gap_describe</code>", "Xem tổng quan nhánh (<code>L0</code>) hoặc bung SDL chi tiết (<code>L2</code>).", "Chỉ trả về mô tả khớp <code>desc_hash</code> đã duyệt (<code>INV-08</code>)."],
                        ["<code>gap_query</code>", "Thực thi truy vấn đọc <code>GAP-QL</code> có gom batch.", "Kiểm tra nhãn read-only + khóa PlanCache theo <code>TenantId</code>."],
                        ["<code>gap_mutate</code>", "Thực thi thao tác ghi/thay đổi trạng thái.", "Bắt buộc <code>@sideEffect</code> + duyệt 2 người trên Cedar nếu không thể hoàn tác."],
                        ["<code>gap_task</code>", "Khởi tạo, theo dõi hoặc hủy tác vụ A2A dài hạn.", "Lưu chuyển trạng thái FSM vào SQLite trước khi phát sự kiện (<code>INV-13</code>)."],
                    ],
                )
                + """<p>Cấu hình Claude Desktop, Cursor hoặc Antigravity dùng chế độ stdio (<code>gap serve --stdio</code>) hoặc gọi trực tiếp qua HTTP (<code>POST /mcp</code>):</p>"""
                + code_block(
                    '{\n  "mcpServers": {\n    "gap-gateway": {\n      "command": "/usr/local/bin/gap",\n      "args": ["serve", "--config", "/etc/gap/gap.toml", "--stdio"]\n    }\n  }\n}',
                    "json",
                    "host-mcp-stdio",
                    "vi",
                    title="mcp_servers.json — Cấu hình Client Stdio",
                )
                + code_block(
                    'curl -sS -X POST http://127.0.0.1:8080/mcp \\\n  -H "Content-Type: application/json" \\\n  -H "Accept: application/json, text/event-stream" \\\n  -H "MCP-Protocol-Version: 2026-07-28" \\\n  -H "Mcp-Method: tools/call" \\\n  -H "Mcp-Name: gap_query" \\\n  -H "Authorization: Bearer gap_dev_skeleton_key_01" \\\n  -d \'{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}},"name":"gap_query","arguments":{"query":"{ customer(id: 2) { id name } customer_notes(customer_id: 2) }"}}}\'',
                    "terminal",
                    "host-mcp-http",
                    "vi",
                    title="Terminal — Gọi MCP 2026-07-28 qua HTTP",
                ),
            },
            {
                "id": "a2a-and-agui",
                "h2": "Liên thông A2A v1.0 & Luồng Giao diện AG-UI v1.0",
                "subheadings": [],
                "html": """<ul>
  <li><strong>Chuẩn ngang hàng A2A v1.0 (<code>adapters/gap-a2a</code>):</strong> Quản lý vòng đời tác vụ 8 trạng thái trên SQLite kèm bộ đếm hop tăng đơn điệu, ngắt ngay vòng lặp chuyển tiếp (<code>GAP_LOOP_DETECTED</code>, <code>INV-20</code>).</li>
  <li><strong>Luồng sự kiện AG-UI v1.0 (<code>adapters/gap-agui</code>):</strong> Mở <code>POST /ag-ui</code> SSE phát chuẩn <code>RUN_STARTED</code>, <code>STEP_STARTED</code>, <code>TOOL_CALL_*</code>, <code>STATE_SNAPSHOT</code>, <code>RUN_FINISHED</code>. Khi Cedar yêu cầu con người duyệt mutation <code>IRREVERSIBLE</code>, luồng dừng lại qua <code>RUN_FINISHED</code> interrupt và chạy tiếp khi có xác nhận.</li>
</ul>"""
                + code_block(
                    'curl -sS -N -X POST http://127.0.0.1:8080/ag-ui \\\n  -H "Content-Type: application/json" \\\n  -H "Accept: text/event-stream" \\\n  -H "Authorization: Bearer gap_dev_skeleton_key_01" \\\n  -d \'{"threadId":"demo-1","runId":"r-1","messages":[],"forwardedProps":{"graphql":{"query":"{ customer(id: 1) { id name tier } customer_notes(customer_id: 1) }"}}}\'',
                    "terminal",
                    "host-agui-curl",
                    "vi",
                    title="Terminal — Nhận Luồng SSE AG-UI v1.0",
                )
                + code_block(
                    'data: {"type":"RUN_STARTED","threadId":"demo-1","runId":"r-1","metadata":{"generation":1,"tenant":"default"}}\ndata: {"type":"TOOL_CALL_RESULT","toolCallName":"gap_query",...}\ndata: {"type":"STATE_SNAPSHOT","snapshot":{"generation":1,"lastData":{"customer":{"id":1,"name":"Ada Lovelace","tier":"vip"},...}}}\ndata: {"type":"RUN_FINISHED","outcome":{"type":"success"}}',
                    "text",
                    "host-agui-out",
                    "vi",
                    title="Chuỗi Sự kiện SSE Mong đợi",
                ),
            },
        ],
        "markdown": """# Tích hợp MCP, A2A & AG-UI

Một gateway GAP duy nhất đồng thời phục vụ:
1. **MCP `2026-07-28` (`gap-mcp`):** 5 static tools cố định (`651` tokens) qua HTTP hoặc `--stdio`.
2. **A2A `v1.0` (`gap-a2a`):** AgentCard 8 trạng thái, SQLite task leases (`INV-13`) và chống vòng lặp (`INV-20`).
3. **AG-UI `v1.0` (`gap-agui`):** `POST /ag-ui` SSE, `StateDelta`, human-in-the-loop interrupts và thẻ `<gap:untrusted>`.
""",
    }

    # 9. ROUTERS & HTTP/3
    u, _ = _ctx("routers-and-http3")
    pages["routers-and-http3"] = {
        "lead": (
            "GAP tách biệt hoàn toàn các mô hình quyết định System-1 tùy chọn (`@receptron/laya`, TypeSafe `Jev`, Google `gemini-decision-1`) khỏi đường thực thi nóng, đồng thời hỗ trợ HTTP/2 + HTTP/3 QUIC tắt `0-RTT`."
        ),
        "learn_bullets": [
            "Vì sao GAP vận hành mặc định với `BASELINE_FALLBACK` thay vì mô hình neural chưa tinh chỉnh (`G-S1`).",
            "Cách bật nhà cung cấp `[router]` có kiểm soát egress và che bí mật.",
            "Cấu hình HTTP/3 QUIC (`RFC 9114`) an toàn với chế độ `prod` fail-closed TLS.",
        ],
        "sections": [
            {
                "id": "baseline-vs-s1",
                "h2": "Định tuyến System-1 & Cổng Kiểm định `G-S1`",
                "subheadings": [],
                "html": """<p>Một mô hình neural nhỏ chỉ hữu ích khi vượt qua được bộ tìm kiếm từ vựng + cấu trúc cực nhanh. Kết quả đo trên bộ chuẩn <code>routebench</code> (<code>docs/decisions/G-S1.md</code>):</p>"""
                + data_table(
                    "So sánh Thực nghiệm trên RouteBench (G-S1)",
                    ["Bộ định tuyến", "Top-1 (Held-Out / Test)", "Top-1 Debiased", "Độ trễ p95", "Trạng thái Mặc định"],
                    [
                        ["<strong>Baseline (BM25 + Cấu trúc)</strong>", "<strong>97,22% / 95,24%</strong>", "Mốc chuẩn", "<strong>0,887 ms</strong>", "<strong>Bật mặc định</strong>"],
                        ["<code>laya-candle:f32</code> (zero-shot)", "Không đạt cổng G-S1", "14,29%", "73,85 ms", "Chỉ bật khi chọn trong <code>[router]</code> (<code>INV-17</code>)"],
                    ],
                )
                + callout(
                    "important",
                    "An toàn Dự phòng Mở INV-17",
                    "<p>Nếu bất kỳ provider System-1 nào (<code>@receptron/laya</code>, TypeSafe <code>jev</code> <code>/v1/systemone</code> hoặc Google <code>gemini_decision_1</code>) bị tắt, quá hạn hay trả định dạng lỗi, hệ thống tự động rơi về bộ định tuyến nền tất định mà không bao giờ làm sập request hay ảnh hưởng Cedar.</p>",
                )
                + code_block(
                    '[router]\n# disabled | laya_candle | laya_ort | http_sidecar | receptron_laya | jev | gemini_decision_1\nprovider = "gemini_decision_1"\nendpoint = "https://generativelanguage.googleapis.com"\nmodel = "gemini-decision-1"\napi_key_secret = "env://GEMINI_API_KEY"\ntimeout_ms = 400',
                    "toml",
                    "rt-s1-toml",
                    "vi",
                    title="gap.toml — Cấu hình [router] Tùy chọn",
                ),
            },
            {
                "id": "http3-dual-stack",
                "h2": "Dual-Stack HTTP/2 + HTTP/3 trên QUIC (`OPS-005`, `ADR-026`)",
                "subheadings": [],
                "html": """<p>Khi khai báo trong <code>gap.toml</code>, crate <code>gap-transport</code> mở song song TCP HTTPS (<code>HTTP/1.1</code> + <code>HTTP/2</code> kèm header <code>Alt-Svc: h3=":8443"; ma=86400</code>) và UDP QUIC (<code>quinn</code> + <code>h3</code>, TLS 1.3 ALPN <code>h3</code>):</p>"""
                + code_block(
                    'profile = "prod"\n\n[server]\nlisten = "0.0.0.0:8443"\nhttp3_listen = "0.0.0.0:8443"\ntls_cert_secret = "file:///etc/gap/cert.pem"\ntls_key_secret = "file:///etc/gap/key.pem"',
                    "toml",
                    "rt-h3-toml",
                    "vi",
                    title="gap.toml — Dual-Stack HTTP/2 + HTTP/3",
                )
                + """<ul>
  <li><strong>Chống Replay Attack:</strong> Thiết lập <code>max_early_data_size = 0</code> để tắt hoàn toàn TLS 1.3 <code>0-RTT</code> trên QUIC, ngăn chặn phát lại các thao tác thay đổi dữ liệu.</li>
  <li><strong>Fail-Closed ở <code>prod</code>:</strong> Nếu chạy <code>profile = "prod"</code> có <code>http3_listen</code> nhưng thiếu chứng chỉ <code>tls_cert_secret</code> / <code>tls_key_secret</code>, cổng từ chối khởi động ngay; ở <code>profile = "dev"</code>, hệ thống tự sinh chứng chỉ TLS 1.3 tự ký tạm thời nếu chưa truyền file chứng chỉ.</li>
  <li><strong>Hiệu năng đo thực tế:</strong> Đạt <strong>5.013,1 req/s</strong> qua kết nối QUIC đa luồng (<strong>1,626 ms</strong> p50 / <strong>2,364 ms</strong> p95 / <code>0</code> lỗi) song hành cùng <strong>1.063,6 req/s</strong> qua TCP HTTP/1.1+2 (<code>11,544 ms</code> p95).</li>
</ul>""",
            },
        ],
        "markdown": """# Định tuyến System-1 & HTTP/3 QUIC

## System-1 so với Baseline (`G-S1`, `INV-17`)
- Baseline từ vựng + cấu trúc đạt **97,22%** held-out / **95,24%** test top-1 ở **0,887 ms** p95 (mặc định).
- Các adapter `[router]` tùy chọn (`laya_candle`, `laya_http`, `jev`, `gemini_decision_1`) luôn có cơ chế fallback an toàn về baseline (`INV-17`).

## HTTP/3 QUIC (`OPS-005`, `ADR-026`)
- Chạy song song TCP HTTP/1.1+2 (`Alt-Svc`) và UDP QUIC `h3` (`quinn` + `h3`).
- Tắt hoàn toàn `0-RTT` (`max_early_data_size = 0`) chống replay mutation.
""",
    }

    # 10. ZERO-TRUST SECURITY
    u, _ = _ctx("zero-trust")
    pages["zero-trust"] = {
        "lead": (
            "GAP coi mọi mô tả công cụ bên ngoài, dữ liệu upstream trả về và địa chỉ mạng đi ra là chưa đáng tin — áp dụng phân quyền Cedar default-deny, ghim desc_hash chống rug-pull, chặn SSRF, OS sandbox và nhật ký băm BLAKE3."
        ),
        "learn_bullets": [
            "Phân tầng tin cậy `T0`, `T1`, `T2` và viết luật Cedar yêu cầu phê duyệt 2 người.",
            "Chống MCP Rug-Pull với `DriftMonitor` và nhốt dữ liệu lạ trong `<gap:untrusted>`.",
            "Chặn triệt để kỹ thuật SSRF DNS-rebinding & metadata đám mây (`169.254.169.254`).",
        ],
        "sections": [
            {
                "id": "trust-tiers-cedar",
                "h2": "Phân bậc Tin cậy & Luật Cedar (`INV-04`, `INV-11`)",
                "subheadings": [],
                "html": """<p>Mọi nhánh upstream đều thuộc đúng một trong ba cấp độ tin cậy tường minh:</p>
<ul>
  <li><strong><code>T0</code> (Chữ ký nội bộ):</strong> Đã xác minh chữ ký Ed25519 trong <code>branch.manifest.json</code> bằng khóa công khai của tổ chức. Sai lệch chữ ký lập tức hạ nhánh xuống <code>T2</code>.</li>
  <li><strong><code>T1</code> (Admin duyệt):</strong> Đã được kiểm duyệt và ghim mã băm schema (<code>spec_hash</code>) cùng mô tả thao tác (<code>desc_hash</code>).</li>
  <li><strong><code>T2</code> (Chưa tin cậy / Bên ngoài):</strong> Tiến trình thực thi CLI/MCP stdio bắt buộc chạy trong OS sandbox (<code>Bubblewrap</code> / <code>nsjail</code> trên Linux, <code>Seatbelt</code> trên macOS) — từ chối chạy nếu thiếu sandbox — và bị cấm join chéo miền tin cậy (<code>INV-07</code>).</li>
</ul>
<p>Phân quyền do bộ đánh giá Cedar thuần hàm (<code>policies/default.cedar</code>) đảm nhiệm, trong đó luật <code>forbid</code> luôn thắng <code>permit</code>:</p>"""
                + code_block(
                    '@id("forbid-mutate-irreversible-or-unknown-unapproved")\nforbid (\n    principal,\n    action == Action::"Mutate",\n    resource\n) when {\n    (resource.sideEffect == "IRREVERSIBLE" || resource.sideEffect == "UNKNOWN")\n        && !(context.approved && context.approverIsDifferent)\n};',
                    "cedar",
                    "sec-cedar",
                    "vi",
                    title="policies/default.cedar — Luật Duyệt 2 Người",
                )
                + code_block(
                    "./target/dist/gap policy check --dir policies\n./target/dist/gap audit verify --nightly",
                    "terminal",
                    "sec-cli-verify",
                    "vi",
                    title="Terminal — Kiểm tra Cedar & Chuỗi BLAKE3",
                )
                + """<ul>
  <li><strong>Không mô hình ML nào can thiệp AuthZ:</strong> Mọi đánh giá Cedar hoàn toàn tất định (<code>INV-04</code>) và bỏ qua hoàn toàn các gợi ý tự khai báo từ server ngoài như <code>readOnlyHint: true</code>.</li>
</ul>""",
            },
            {
                "id": "anti-rugpull-ssrf",
                "h2": "Chống Rug-Pull, Bọc Dữ liệu T2 & Phòng vệ SSRF",
                "subheadings": [],
                "html": data_table(
                    "Các cơ chế bảo mật cấp giao thức được kiểm thử trong tests/security/",
                    ["Mối đe dọa", "Cơ chế Bảo vệ trong GAP", "Hành vi Cô lập Tất định"],
                    [
                        ["<strong>MCP Tool Rug Pull</strong>", "<code>DriftMonitor</code> (<code>SEC-082</code>, <code>INV-08</code>)", "Tính lại JCS BLAKE3 <code>desc_hash</code>; sai lệch lập tức đóng băng nhánh sang <code>BranchStatus::Drift</code> và giữ nguyên mô tả đã ghim."],
                        ["<strong>Prompt Injection Gián tiếp</strong>", "<code>&lt;gap:untrusted&gt;</code> (<code>SEC-080</code>)", "Bọc mọi chuỗi từ nhánh T1/T2 và escape HTML các thẻ đóng giả mạo bên trong payload."],
                        ["<strong>SSRF / DNS Rebinding</strong>", "<code>EgressGuard</code> (<code>SEC-075</code>)", "Phân giải DNS 1 lần, chặn loopback/RFC1918/IP bát phân/<code>169.254.169.254</code>, ghim kết nối vào IP sạch và cấm redirect."],
                        ["<strong>Rò rỉ Token / Bí mật</strong>", "<code>Secret&lt;T&gt;</code> (<code>INV-15</code>)", "Tự động che bí mật khỏi snapshot, log, trace OTel, metrics và thông báo lỗi trả về client."],
                        ["<strong>Giả mạo Nhật ký Audit</strong>", "Chuỗi băm BLAKE3 (<code>INV-14</code>)", "Mọi mutation, phê duyệt và thay đổi policy đều nối chuỗi mật mã BLAKE3 kiểm chứng qua <code>gap audit verify</code>."],
                    ],
                ),
            },
        ],
        "markdown": """# Bảo mật Zero-Trust & Cedar

- **Cedar Default-Deny (`INV-04`, `INV-11`):** Luật `forbid` luôn thắng; thao tác `IRREVERSIBLE` bắt buộc `approverIsDifferent == true`.
- **Chống Rug-Pull (`SEC-082`, `INV-08`):** `DriftMonitor` đóng băng nhánh về `BranchStatus::Drift` khi mô tả bị sửa lén.
- **EgressGuard (`SEC-075`):** Ghim IP sau 1 lần phân giải DNS, chặn `169.254.169.254` và HTTP redirect.
- **Audit Ledger (`INV-14`):** Chuỗi băm BLAKE3 xuất chuẩn W3C PROV-O JSON-LD.
""",
    }

    # 11. PLAYGROUND
    u, _ = _ctx("playground")
    pages["playground"] = {
        "lead": (
            "Thử nghiệm trực quan biểu đồ đối chứng token `GAP-Bench`, gửi truy vấn GraphQL & MCP thật tới tiến trình `gap serve`, lưu tiến độ tự học 6 cấp độ và kiểm tra kiến thức với bộ câu hỏi 6 câu."
        ),
        "learn_bullets": [
            "So sánh lượng token tiêu thụ Direct-MCP với GAP theo từng tác vụ và quy mô `N=10..1.000`.",
            "Chuyển từ chế độ mô phỏng sang gọi socket `gap serve` thật qua `python3 web/serve.py`.",
            "Lưu tự động tiến độ 18 đầu mục thực hành trong trình duyệt (dùng chung cho bản Anh & Việt).",
        ],
        "sections": [
            {
                "id": "interactive-lab-suite",
                "h2": "Bộ tính Crossover & Playground Tương tác",
                "subheadings": [],
                "html": """<div class="lab-grid">
  <div class="subcard">
    <h3>3A. Bộ tính Hiệu quả Token (Số liệu đo thật ADR-021)</h3>
    <div class="control-row">
      <label for="calc-suite">Tác vụ:</label>
      <select id="calc-suite">
        <option value="cross_branch_join">cross_branch_join (Truy vấn gộp liên nhánh)</option>
        <option value="api_lookup">api_lookup (Gọi 1 API đơn lẻ)</option>
        <option value="code_graph">code_graph (Hiểu kho mã nguồn, 24 symbols)</option>
      </select>
    </div>
    <div class="control-row">
      <label for="calc-n">Quy mô N:</label>
      <select id="calc-n">
        <option value="10">N = 10 tools (2 nhánh)</option>
        <option value="50">N = 50 tools (5 nhánh)</option>
        <option value="200">N = 200 tools (20 nhánh)</option>
        <option value="1000" selected>N = 1000 tools (100 nhánh)</option>
      </select>
      <label><input id="calc-cached" type="checkbox" checked /> Bật Prompt Cache (0.10x)</label>
    </div>
    <div id="calc-output"></div>
  </div>

  <div class="subcard">
    <h3>3B. Playground GAP-QL &amp; MCP 2026-07-28</h3>
    <div class="control-row">
      <button id="btn-toggle-demo" type="button" class="btn btn-primary">Bật Lab Thực Hành (gap serve)</button>
      <span id="demo-badge" class="pill">Mô phỏng sẵn</span>
    </div>
    <div class="control-row">
      <label for="play-scenario">Kịch bản:</label>
      <select id="play-scenario"></select>
    </div>
    <p id="play-note" class="section-lead"></p>
    <figure class="code-figure">
      <div class="code-toolbar">
        <div class="code-meta">
          <span class="code-lang">request</span>
          <span class="code-title"><code id="play-endpoint"></code></span>
        </div>
        <span class="copy">
          <button type="button" class="copy-button" command="--copy" commandfor="play-request" aria-label="Sao chép payload request mẫu">
            <span class="icon" aria-hidden="true"></span>
          </button>
          <span class="tip" role="status"></span>
        </span>
      </div>
      <pre role="region" aria-label="Payload request mẫu" tabindex="0"><code id="play-request" class="language-json"></code></pre>
    </figure>
    <div class="control-row">
      <button id="btn-run-play" type="button" class="btn btn-primary">Chạy thử kịch bản</button>
      <span id="play-meta" class="section-lead"></span>
    </div>
    <figure class="code-figure">
      <div class="code-toolbar">
        <div class="code-meta">
          <span class="code-lang">response</span>
          <span class="code-title">Kết quả JSON / SSE</span>
        </div>
        <span class="copy">
          <button type="button" class="copy-button" command="--copy" commandfor="play-response" aria-label="Sao chép kết quả phản hồi">
            <span class="icon" aria-hidden="true"></span>
          </button>
          <span class="tip" role="status"></span>
        </span>
      </div>
      <pre role="region" aria-label="Kết quả phản hồi" tabindex="0"><code id="play-response" class="language-json"></code></pre>
    </figure>
  </div>
</div>""",
            },
            {
                "id": "curriculum-checklist",
                "h2": "Lộ trình 6 Cấp độ (Novice đến Master)",
                "subheadings": [],
                "html": """<p>Dùng phím mũi tên trái/phải (<code>focusgroup</code>) hoặc nhấn từng tab bên dưới. Tiến độ của bạn được lưu tự động trên trình duyệt.</p>
<div class="progress-box progress-inline">
  <div><strong>Tiến độ thực hành:</strong> <span id="progress-text">0 / 18</span> bài</div>
  <div class="progress-track" role="progressbar" aria-valuemin="0" aria-valuemax="18" aria-valuenow="0" aria-label="Tiến độ thực hành" id="progress-bar-wrap">
    <div id="progress-fill" class="progress-fill"></div>
  </div>
</div>
<div id="levels-tablist" class="tablist" focusgroup="tablist nomemory" aria-label="6 cấp độ học GAP"></div>
<div id="levels-tabpanels" class="tabpanels"></div>""",
            },
            {
                "id": "knowledge-quiz",
                "h2": "Tự kiểm tra Kiến thức (6 Câu hỏi Cốt lõi)",
                "subheadings": [],
                "html": """<div id="quiz-container"></div>
<div id="quiz-score" class="pill">Điểm hiện tại: 0 / 6</div>""",
            },
        ],
        "markdown": """# Phòng Lab & Bộ tính Token

Trang thực hành tương tác (`vi/lab/playground.html`) giúp bạn:
- Thử nghiệm bộ tính Crossover (`GAP-Bench`, `N=10..1.000`, bật/tắt Prompt Cache)
- Chạy truy vấn sống tới `gap serve` khi bật `python3 web/serve.py --port 8787`
- Theo dõi 18 bước tự luyện từ Novice đến Master
- Kiểm tra kiến thức qua 6 câu hỏi cốt lõi
""",
    }

    # 12. CLI & OPS
    u, _ = _ctx("cli-and-ops")
    pages["cli-and-ops"] = {
        "lead": (
            "Vận hành GAP bằng một file binary 28,1 MB duy nhất gồm 15 lệnh con CLI, bảng mã thoát chuẩn hóa (`OPS-044`), 9 runbook xử lý cảnh báo Prometheus và bản đồ 32 crate Rust phân tầng."
        ),
        "learn_bullets": [
            "Tra cứu trọn bộ 15 lệnh con `gap` và chạy thử `--help` trực tiếp khi dùng `serve.py`.",
            "Xem 9 Runbook xử lý sự cố Prometheus Alerts kèm nút Mở/Thu gọn tất cả.",
            "Lọc bản đồ 32 crate Rust theo từng tầng kiến trúc (`L0` đến `L6` và `Dev`).",
        ],
        "sections": [
            {
                "id": "cli-and-runbooks",
                "h2": "15 Lệnh CLI & 9 Runbooks Xử lý Sự cố",
                "subheadings": [],
                "html": """<p>Mã thoát chuẩn của binary <code>gap</code> (<code>OPS-044</code>): <code>0</code> thành công · <code>1</code> lỗi runtime · <code>2</code> sai cấu hình · <code>3</code> từ chối bảo mật/audit · <code>4</code> không tìm thấy.</p>
<div class="lab-grid">
  <div class="subcard">
    <h3>15 Lệnh con của Binary <code>gap</code></h3>
    <div id="cli-list" class="accordion"></div>
  </div>
  <div class="subcard">
    <div class="accordion-header">
      <h3>9 Runbooks Vận hành (Prometheus Alerts)</h3>
      <button id="btn-toggle-all-runbooks" type="button" class="btn btn-sm" commandfor="runbooks-accordion" command="--toggle-all" aria-expanded="false">
        <span class="expand">Mở tất cả</span>
        <span class="collapse">Thu gọn tất cả</span>
      </button>
    </div>
    <section id="runbooks-accordion" class="accordion" aria-label="Danh sách 9 Runbook xử lý sự cố"></section>
  </div>
</div>""",
            },
            {
                "id": "crate-layer-map",
                "h2": "Bản đồ Kiến trúc 32 Crates Rust (Phân tầng L0-L6)",
                "subheadings": [],
                "html": """<p>Mỗi crate ở tầng cao hơn chỉ được phụ thuộc xuống tầng thấp hơn (<code>tools/check_layers.py</code>). Bấm vào từng tầng để lọc nhanh:</p>
<div id="layer-chips" class="chip-group" role="group" aria-label="Lọc crate theo tầng kiến trúc"></div>
<div id="crates-grid" class="crates-grid"></div>""",
            },
        ],
        "markdown": """# Tra cứu CLI, Runbooks & 32 Crates

## Bảng Mã thoát Chuẩn (`OPS-044`)
- `0`: thành công
- `1`: lỗi runtime
- `2`: lỗi cấu hình / tham số
- `3`: từ chối bảo mật hoặc sai lệch chuỗi audit
- `4`: không tìm thấy tài nguyên

Xem danh sách tương tác toàn bộ 15 lệnh con CLI, 9 runbook và 32 crate trong `vi/reference/cli-and-ops.html`.
""",
    }

    # 13. INVARIANTS & GLOSSARY
    u, a = _ctx("invariants-and-glossary")
    pages["invariants-and-glossary"] = {
        "lead": (
            "Tra cứu trọn bộ 20 bất biến kiến trúc (`INV-01..20`), từ điển kiểm soát thuật ngữ chuẩn ASD-STE100, trích dẫn BibTeX và toàn văn bài báo khoa học bản PDF/HTML."
        ),
        "learn_bullets": [
            "Kiểm chứng từng bất biến `INV-01..20` gắn với mã test truy vết tự động trong kho.",
            "Áp dụng từ điển thuật ngữ ASD-STE100 (`dist/check-ste100.py`) khi viết đặc tả hay prompt.",
            "Tải bài báo PDF, đọc bản HTML hoặc sao chép trích dẫn BibTeX chính thức.",
        ],
        "sections": [
            {
                "id": "invariants-matrix",
                "h2": "Bảng 20 Bất biến Kiến trúc (`INV-01` – `INV-20`)",
                "subheadings": [],
                "html": """<div tabindex="0" role="region" aria-labelledby="cap-inv" class="table-wrapper">
  <table class="data-table inv-table">
    <caption id="cap-inv">20 bất biến chuẩn tắc được kiểm tra tự động trong GAP</caption>
    <thead>
      <tr>
        <th scope="col">Mã</th>
        <th scope="col">Tên &amp; Yêu cầu</th>
        <th scope="col">Mặt phẳng / Crate thực thi</th>
        <th scope="col">Trạng thái kiểm chứng</th>
      </tr>
    </thead>
    <tbody id="inv-tbody"></tbody>
  </table>
</div>""",
            },
            {
                "id": "ste100-dictionary",
                "h2": "Từ điển Kiểm soát Chuẩn ASD-STE100",
                "subheadings": [],
                "html": """<p>Để tránh LLM tự bịa từ đồng nghĩa khi đọc/viết tài liệu kỹ thuật, bộ <code>docs/ste100/</code> và <code>dist/check-ste100.py</code> quy định mỗi khái niệm mang một tên chuẩn duy nhất và giới hạn câu thủ tục tối đa 20 từ:</p>
<div tabindex="0" role="region" aria-labelledby="cap-dict" class="table-wrapper">
  <table class="data-table dict-table">
    <caption id="cap-dict">Thuật ngữ chuẩn ASD-STE100 trong GAP</caption>
    <thead>
      <tr>
        <th scope="col">Thuật ngữ chuẩn</th>
        <th scope="col">Nghĩa trong GAP</th>
        <th scope="col">Từ cấm (không dùng thay thế)</th>
      </tr>
    </thead>
    <tbody id="dict-tbody"></tbody>
  </table>
</div>""",
            },
            {
                "id": "paper-and-citation",
                "h2": "Bài báo Khoa học & Trích dẫn Học thuật",
                "subheadings": [],
                "html": f"""<div class="hero-actions">
  <a class="btn btn-primary" href="{esc(a('paper/gap-paper.html'))}" target="_blank" rel="noopener">Đọc Bài báo (HTML)</a>
  <a class="btn" href="{esc(a('paper/gap-paper.pdf'))}" target="_blank" rel="noopener">Tải Bài báo PDF</a>
  <a class="btn" href="{esc(a('CITATION.cff'))}" target="_blank" rel="noopener">CITATION.cff</a>
</div>"""
                + code_block(
                    '@article{nguyen2026gap,\n  author  = {Nguy{\\~e}n, Duy {\\DH}{\\\'u}c},\n  title   = {Graph Agent Protocol (GAP): Unifying Typed Discovery, Batched Federation, and Zero-Trust Governance for AI Agents},\n  year    = {2026},\n  note    = {Reference Release v0.1.0-rc.1; Infinite Agent Labs; ORCID: 0009-0009-4741-5637},\n  url     = {https://github.com/vnknowledge2014/GraphProtocol}\n}',
                    "bibtex",
                    "ref-bibtex",
                    "vi",
                ),
            },
        ],
        "markdown": """# 20 Bất biến, ASD-STE100 & Bài báo Khoa học

- **20 Bất biến Kiến trúc (`INV-01..20`):** Được kiểm chứng qua 2.516 test markers bằng `tools/spec_trace.py --strict`.
- **Từ điển Chuẩn ASD-STE100:** Kiểm tra tự động qua `python3 dist/check-ste100.py`.
- **Tác giả:** Duy Đức Nguyễn (Infinite Agent Labs, ORCID `0009-0009-4741-5637`).
- **Kho mã nguồn:** `https://github.com/vnknowledge2014/GraphProtocol`
""",
    }

    return pages
