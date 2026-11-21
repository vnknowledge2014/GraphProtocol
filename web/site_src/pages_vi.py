"""Vietnamese documentation pages 1..7 following Apollo GraphQL Docs pedagogical structure."""
from __future__ import annotations

from .diagrams import (
    svg_batch_join,
    svg_mental_model,
    svg_progressive_levels,
    svg_three_planes,
)
from .nav import link_to_asset, link_to_page
from .render import callout, code_block, data_table, esc, metric_bar_chart, steps_list


def _ctx(slug: str):
    u = lambda target: link_to_page(slug, target, "vi")
    a = lambda asset: link_to_asset(slug, "vi", asset)
    return u, a


def build_vi_part1() -> dict[str, dict]:
    pages: dict[str, dict] = {}

    # 1. HOME
    u, a = _ctx("home")
    pages["home"] = {
        "lead": (
            "Graph Agent Protocol (GAP) là cổng gateway GraphQL định kiểu chặt chẽ kèm tầng quản trị zero-trust viết bằng Rust (32 crates phân tầng). "
            "Hợp nhất OpenAPI REST, SQL, GraphQL, gRPC, CLI, đồ thị mã nguồn AST, MCP server và agent A2A sau một giao diện lũy tiến thống nhất."
        ),
        "learn_bullets": [
            "Kết nối hàng trăm công cụ mà không làm phình ngữ cảnh LLM (chỉ 651 tokens thay vì 203.421 tokens).",
            "Dập tắt lỗi vòng lặp N+1 qua gom lô DataLoader (INV-18) và snapshot bất biến ArcSwap.",
            "Cưỡng chế bảo mật mặc định từ chối Cedar, chống rug-pull mô tả tool và chặn SSRF qua EgressGuard.",
        ],
        "sections": [
            {
                "id": "get-started-paths",
                "h2": "Chọn Lộ trình Học của Bạn",
                "subheadings": [],
                "html": f"""<div class="stat-strip">
  <div class="stat-item"><span class="stat-num">651</span><span class="stat-label">tokens cố định (thay vì 203.421 Direct-MCP)</span></div>
  <div class="stat-item"><span class="stat-num">99%</span><span class="stat-label">giảm token ở N=1.000 tools</span></div>
  <div class="stat-item"><span class="stat-num">5.013</span><span class="stat-label">req/s QUIC thực nghiệm</span></div>
  <div class="stat-item"><span class="stat-num">28,86 ms</span><span class="stat-label">phục hồi sau SIGKILL</span></div>
  <div class="stat-item"><span class="stat-num">32</span><span class="stat-label">crates Rust phân tầng, zero ML trên hot path</span></div>
</div>
<p>Chọn điểm khởi đầu phù hợp với nhu cầu. Mỗi lộ trình tập trung lý thuyết trước, thực hành sau với lệnh chạy được ngay từ kho mã nguồn.</p>
<div class="track-grid">
  <a class="track-card track-card--beginner" href="{esc(u('learn-graphql-and-gap'))}">
    <span class="track-badge track-badge--beginner">Người mới · 8 phút</span>
    <h3>Học GraphQL &amp; GAP Cơ bản</h3>
    <p>Trực giác từ gốc: ẩn dụ Supergraph, Kiểu &amp; Scalar, quy tắc <code>!</code> và Bộ dựng Truy vấn tương tác 1:1.</p>
  </a>
  <a class="track-card track-card--intermediate" href="{esc(u('academy-hub'))}">
    <span class="track-badge track-badge--intermediate">Trung cấp · 4 Khóa học</span>
    <h3>Học viện GAP (16 Bài thực chiến)</h3>
    <p>Siêu đồ thị đầu tiên, MCP/AG-UI, Gom lô REST/SQL N+1 và Bảo mật Zero-Trust Enterprise.</p>
  </a>
  <a class="track-card track-card--expert" href="{esc(u('architecture'))}">
    <span class="track-badge track-badge--expert">Chuyên sâu · Kiến trúc</span>
    <h3>Kiến trúc &amp; Đặc tả Kỹ thuật</h3>
    <p>Ba mặt phẳng nội bộ, ArcSwap snapshot zero-copy, Cedar zero-trust, HTTP/3 QUIC và 20 bất biến.</p>
  </a>
</div>
<p style="margin-top:0.5rem;">Đã nắm cơ bản? Nhảy thẳng vào <a href="{esc(u('quickstart'))}">Khởi chạy Nhanh 10 phút</a> hoặc <a href="{esc(u('playground'))}">Phòng Lab &amp; Bộ tính Crossover</a>.</p>""",
            },
            {
                "id": "core-capabilities",
                "h2": "4 Vấn đề GAP Giải quyết",
                "subheadings": [],
                "html": f"""<div class="problem-grid">
  <div class="problem-card">
    <div class="prob-icon">📦</div>
    <h3>Bùng nổ Ngữ cảnh Prompt</h3>
    <p>1.000 tools JSON = <strong>203.421 tokens</strong> trước khi người dùng kịp đặt câu hỏi. GAP giữ cố định ở <strong>651 tokens</strong>.</p>
  </div>
  <div class="problem-card">
    <div class="prob-icon">🔁</div>
    <h3>Vòng lặp N+1 Thác nước</h3>
    <p>Không có bộ lập kế hoạch gom khóa, agent lặp từng hàng: lấy khách → lặp đơn hàng → lấy ghi chú. Lãng phí latency và tokens.</p>
  </div>
  <div class="problem-card">
    <div class="prob-icon">🎭</div>
    <h3>Mô tả Tool Biến đổi (Rug Pull)</h3>
    <p>MCP server bên ngoài sửa mô tả sau khi admin duyệt để gài lệnh độc hại. Client phẳng chuyển thẳng cho LLM.</p>
  </div>
  <div class="problem-card">
    <div class="prob-icon">🔓</div>
    <h3>Phân quyền Xác suất Kém An toàn</h3>
    <p>Neural classifier trên đường phân quyền luôn tiềm ẩn rủi ro qua mặt. Phân quyền bắt buộc phải là mã tất định.</p>
  </div>
</div>
<h3 style="margin:2rem 0 1rem;font-size:1.1rem;">Các Trụ cột Kỹ thuật Cốt lõi</h3>
<div class="pillar-grid">
  <a class="pillar-card" href="{esc(u('architecture'))}">
    <h3>Kiến trúc 3 Mặt phẳng</h3>
    <p>Đường thực thi nóng không ML (<code>15 ns</code> ghim <code>ArcSwap</code>, AVL <code>OrdMap</code> tự chủ) tách bạch hoàn toàn khỏi khám phá và kiểm soát.</p>
  </a>
  <a class="pillar-card" href="{esc(u('progressive-disclosure'))}">
    <h3>Khám phá Lũy tiến (651 Tokens)</h3>
    <p>Thay thế việc tải tuyến tính <code>203.421</code> token bằng đúng 5 MCP tool tĩnh qua 3 tầng (<code>L0 -&gt; L1 -&gt; L2</code>).</p>
  </a>
  <a class="pillar-card" href="{esc(u('connectors-overview'))}">
    <h3>6 Native Graph Connectors</h3>
    <p>Ánh xạ khai báo OpenAPI, SQL, GraphQL, gRPC, CLI sandbox và Code AST qua cổng duyệt Propose-Not-Activate.</p>
  </a>
  <a class="pillar-card" href="{esc(u('facades-and-hosts'))}">
    <h3>Facade MCP, A2A &amp; AG-UI</h3>
    <p>Phục vụ đồng thời MCP <code>2026-07-28</code>, tác vụ ngang hàng A2A <code>v1.0</code> và luồng duyệt AG-UI <code>v1.0</code> <code>/ag-ui</code> SSE.</p>
  </a>
  <a class="pillar-card" href="{esc(u('zero-trust'))}">
    <h3>Bảo mật Zero-Trust &amp; Cedar</h3>
    <p>Phân tầng <code>T0/T1/T2</code>, bọc <code>&lt;gap:untrusted&gt;</code>, đóng băng rug-pull <code>DriftMonitor</code>, chặn SSRF và chuỗi băm BLAKE3.</p>
  </a>
  <a class="pillar-card" href="{esc(u('routers-and-http3'))}">
    <h3>HTTP/3 QUIC &amp; Định tuyến System-1</h3>
    <p>5.013 req/s HTTP/3 QUIC (<code>2,36 ms</code> p95, tắt <code>0-RTT</code>) cùng bộ định tuyến nền <code>95,24%</code> và fallback <code>INV-17</code>.</p>
  </a>
</div>""",
            },
            {
                "id": "measured-highlights",
                "h2": "Số liệu Thực nghiệm Đã Kiểm chứng (`ADR-021`)",
                "subheadings": [],
                "html": f"""<div class="compare-row">
  <div class="compare-before">
    <h3>❌ Không có GAP (Direct-MCP)</h3>
    <ul style="margin:0;padding-left:1.25rem;font-size:0.9rem;">
      <li><strong>203.421 tokens</strong> chỉ để liệt kê 1.000 tools</li>
      <li>Agent lặp N+1 cho từng khách hàng/đơn hàng/ghi chú</li>
      <li>Mô tả tool có thể bị sửa đổi sau khi duyệt</li>
      <li>Neural classifier trên đường phân quyền hot-path</li>
    </ul>
  </div>
  <div class="compare-after">
    <h3>✅ Với GAP</h3>
    <ul style="margin:0;padding-left:1.25rem;font-size:0.9rem;">
      <li><strong>651 tokens</strong> cố định (5 tool tĩnh)</li>
      <li>Khai báo <code>@join</code> + gom lô DataLoader</li>
      <li>BLAKE3 <code>desc_hash</code> pinning đóng băng drift</li>
      <li>Cedar default-deny tất định, không xác suất</li>
    </ul>
  </div>
</div>
<p>Toàn bộ thông số dưới đây trích trực tiếp từ các bộ test trong kho mã nguồn, <code>bench/results/gapbench/baseline.json</code> và <code>bench/results/ga_local/report.json</code>.</p>
<div class="kpi-grid">
  <div class="kpi"><div class="val">651 tokens</div><div class="lbl">Schema MCP <code>tools/list</code> cố định từ N=10 tới 1.000 tools (Direct-MCP: 203.421)</div></div>
  <div class="kpi"><div class="val">99,0% / 208,7x</div><div class="lbl">Tiết kiệm token join liên nhánh ở N=1.000 &amp; duyệt Code Graph (24 symbols)</div></div>
  <div class="kpi"><div class="val">5.013 req/s</div><div class="lbl">HTTP/3 QUIC đa luồng (2,36 ms p95, 0 lỗi, tắt 0-RTT) kèm 1.064 req/s TCP HTTP/2</div></div>
  <div class="kpi"><div class="val">28,86 ms</div><div class="lbl">Phục hồi /readyz sau SIGKILL (kill -9) · 9,32 MiB idle RSS · binary 28,1 MB tái lập</div></div>
  <div class="kpi"><div class="val">87 / 87 WP</div><div class="lbl">100% đặc tả truy vết (2.516 markers) · 136/136 conformance (CI) · 0 RUSTSEC</div></div>
</div>
<div class="hero-actions">
  <a class="btn btn-primary" href="{esc(u('quickstart'))}">Bắt đầu Khởi chạy Nhanh (10 phút)</a>
  <a class="btn" href="{esc(a('paper/gap-paper.html'))}" target="_blank" rel="noopener">Đọc Bài báo Khoa học (HTML)</a>
  <a class="btn" href="{esc(a('paper/gap-paper.pdf'))}" target="_blank" rel="noopener">Tải Bài báo PDF</a>
  <a class="btn" href="https://github.com/vnknowledge2014/GraphProtocol" target="_blank" rel="noopener">Kho mã nguồn GitHub</a>
</div>"""
                + code_block(
                    "./target/dist/gap init my-gateway\n./target/dist/gap config check my-gateway/gap.toml\n./target/dist/gap doctor --config my-gateway/gap.toml",
                    "terminal",
                    "home-init",
                    "vi",
                    title="Thử GAP trong 30 giây — Khởi tạo & Chẩn đoán",
                ),
            },
            {
                "id": "launch-videos",
                "h2": "Video Giới thiệu Dự án (1080p)",
                "subheadings": [],
                "html": f"""<div class="lab-grid videos-grid">
  <div class="subcard">
    <h3 class="video-heading">Video Giới thiệu (Thuyết minh Tiếng Việt · 1080p)</h3>
    <video class="promo-video" controls preload="metadata" playsinline width="1920" height="1080">
      <source src="{esc(a('media/gap-promo-vi.mp4'))}" type="video/mp4" />
      Trình duyệt không hỗ trợ phát MP4 (<a href="{esc(a('media/gap-promo-vi.mp4'))}">tải video Tiếng Việt</a>).
    </video>
  </div>
  <div class="subcard">
    <h3 class="video-heading">Video Giới thiệu (Thuyết minh Tiếng Anh · 1080p)</h3>
    <video class="promo-video" controls preload="metadata" playsinline width="1920" height="1080">
      <source src="{esc(a('media/gap-promo-en.mp4'))}" type="video/mp4" />
      Trình duyệt không hỗ trợ phát MP4 (<a href="{esc(a('media/gap-promo-en.mp4'))}">tải video Tiếng Anh</a>).
    </video>
  </div>
</div>""",
            },
        ],
        "markdown": """# Trang chủ Tài liệu GAP

Graph Agent Protocol (GAP) là cổng gateway GraphQL định kiểu chặt chẽ kèm tầng quản trị zero-trust viết bằng Rust (32 crates phân tầng, Apache-2.0).

## Số liệu Thực nghiệm (ADR-021)
- Chi phí schema MCP tĩnh: cố định **651 tokens** cho N=10..1.000 tools (Direct-MCP: 203.421 tokens)
- Giảm token join liên nhánh (N=1.000): **99,0%** không cache (**93,5%** có cache), rút 4 lượt -> 2 lượt
- Duyệt đồ thị mã nguồn (24 symbols): **119,32x** không cache / **208,68x** có cache, rút 11 lượt -> 1 lượt
- HTTP/3 QUIC (RFC 9114, tắt 0-RTT): **5.013 req/s** (2,36 ms p95) kèm **1.064 req/s** TCP HTTP/2
- Phục hồi sự cố & bảo mật: **28,86 ms** sau SIGKILL, **9,32 MiB** idle RSS, **0** lỗ hổng RUSTSEC
""",
    }

    # 2. WHAT IS GAP?
    u, _ = _ctx("what-is-gap")
    pages["what-is-gap"] = {
        "lead": (
            "GAP cấp cho AI Agent một cổng vào duy nhất, an toàn và định kiểu chặt để gọi REST API, SQL, gRPC, GraphQL, CLI và kho mã nguồn — không cần nhồi hàng trăm nghìn tokens JSON vào prompt."
        ),
        "learn_bullets": [
            "Vì sao cách nạp phẳng toàn bộ schema công cụ vào LLM sẽ vỡ trận khi mở rộng quy mô.",
            "5 thuật ngữ cốt lõi trong GAP: Gateway, Branch, Connector, Snapshot và Principal.",
            "Ngưỡng giao cắt thực tế: khi nào Direct-MCP gọn hơn và khi nào GAP áp đảo.",
        ],
        "sections": [
            {
                "id": "the-four-problems",
                "h2": "Vì sao Mô hình Tool Phẳng Gặp Giới hạn",
                "subheadings": [],
                "html": """<div class="will-learn">
  <h3>Sau trang này bạn sẽ hiểu được</h3>
  <ul>
    <li>Vì sao nạp toàn bộ schema công cụ vào mỗi prompt sẽ vỡ trận ở quy mô sản xuất (tường 203.421 tokens)</li>
    <li>Bốn điểm nghẽn thực tế: bùng nổ token, vòng lặp N+1, rug-pull injection, phân quyền xác suất</li>
    <li>Ngưỡng giao cắt: khi nào Direct-MCP gọn hơn và khi nào GAP áp đảo</li>
  </ul>
</div>
<p>Phần lớn hệ thống agent hiện tại nối thẳng LLM tới hàng loạt MCP hoặc HTTP endpoint rời rạc. Cách này chạy ổn với 5–10 công cụ, nhưng nhanh chóng gặp 4 điểm nghẽn khi đưa vào vận hành thật:</p>
<div class="problem-grid">
  <div class="problem-card">
    <div class="prob-icon">📦</div>
    <h3>Bùng nổ Ngữ cảnh Prompt</h3>
    <p>Tải sẵn 1.000 mô tả JSON Schema tốn tới <strong>203.421 tokens</strong> ngay trước khi người dùng kịp đặt câu hỏi.</p>
  </div>
  <div class="problem-card">
    <div class="prob-icon">🔁</div>
    <h3>Vòng lặp Thác nước N+1</h3>
    <p>Thiếu bộ lập kế hoạch gom khóa, LLM lặp tuần tự từng lượt cho đơn hàng và ghi chú — vừa chậm vừa tốn token.</p>
  </div>
  <div class="problem-card">
    <div class="prob-icon">🎭</div>
    <h3>Mô tả Tool Biến đổi (Rug Pull)</h3>
    <p>MCP server bên ngoài lén sửa mô tả công cụ sau khi admin đã duyệt để gài lệnh độc hại vào prompt LLM.</p>
  </div>
  <div class="problem-card">
    <div class="prob-icon">🔓</div>
    <h3>Phân quyền Xác suất Kém An toàn</h3>
    <p>Để ML hay LLM quyết định quyền truy cập trên hot-path luôn tiềm ẩn rủi ro qua mặt. Phân quyền phải là mã tất định.</p>
  </div>
</div>""" + svg_mental_model("vi"),
            },
            {
                "id": "key-terminology",
                "h2": "5 Khái niệm Cốt lõi Cần Nắm",
                "subheadings": [],
                "html": data_table(
                    "Thuật ngữ chuẩn và ánh xạ mã nguồn trong Graph Agent Protocol",
                    ["Thuật ngữ", "Ý nghĩa dễ hiểu", "Thành phần trong Kho mã nguồn"],
                    [
                        ["<code>gateway</code>", "Binary máy chủ duy nhất lắng nghe HTTP/3 QUIC, HTTP/2 hoặc stdio cho agent gọi vào.", "<code>gap-cli</code> / <code>gap-server</code>"],
                        ["<code>branch</code>", "Một nguồn backend đã đăng ký (API REST, CSDL SQL, CLI, mã nguồn hoặc MCP server).", "Khai báo qua <code>[[branches]]</code> trong <code>gap.toml</code>"],
                        ["<code>connector (NGC)</code>", "Adapter ánh xạ mọi giao thức ngoài về một hợp đồng protobuf + GraphQL thống nhất.", "6 adapter mẫu tại <code>adapters/gap-ngc-*</code>"],
                        ["<code>snapshot</code>", "Gói biên dịch bất biến gồm schema, chỉ mục tìm kiếm và luật Cedar được ghim cho từng request.", "<code>gap-snapshot</code> (<code>ArcSwap</code>)"],
                        ["<code>principal</code>", "Danh tính người dùng hoặc AI Agent đã xác thực được kiểm tra quyền qua Cedar default-deny.", "<code>gap-sec</code> + <code>policies/default.cedar</code>"],
                    ],
                ),
            },
            {
                "id": "when-not-to-use",
                "h2": "Khi nào Direct-MCP vẫn Ít Token hơn (Đối chứng Trung thực)",
                "subheadings": [],
                "html": callout(
                    "tradeoff",
                    "Kết quả Đối chứng Trung thực (ADR-021)",
                    f"""<p>Nếu hệ thống chỉ có <strong>tối đa 50 tool đơn giản dạng đọc 1 bước</strong> và nhà cung cấp LLM giữ <strong>Prompt Prefix Cache 0,10x</strong> luôn ấm, Direct-MCP tốn ít token hơn ở câu gọi đơn (<code>547,2</code> so với <code>4.691,9</code> tokens ở <code>N=10</code>).</p>
<p>Ngược lại, GAP vượt trội hoàn toàn khi số tool tăng từ ~200 trở lên, khi có join đa nhánh, duyệt đồ thị mã nguồn, hoặc khi yêu cầu bảo mật chặt (duyệt mutation Cedar, chống SSRF, đóng băng rug-pull). Xem trực quan tại <a href="{esc(u('playground'))}">Bộ tính Crossover</a>.</p>""",
                ),
            },
        ],
        "markdown": """# GAP là gì?

GAP mang lại một cổng vào duy nhất, định kiểu chặt chẽ cho REST, SQL, gRPC, GraphQL, CLI và đồ thị mã nguồn mà không nhồi schema khổng lồ vào ngữ cảnh LLM.

## 5 Thuật ngữ Cốt lõi
- **Gateway**: Binary Rust phục vụ `/graphql`, `/mcp`, `/ag-ui` và A2A `v1.0`.
- **Branch**: Nguồn upstream đã đăng ký (`shop`, `notes`, ...).
- **Connector (NGC)**: Adapter chuyển giao thức ngoài về GraphQL profile thống nhất.
- **Snapshot**: Ảnh bất biến gồm supergraph, chỉ mục và luật Cedar ghim nguyên tử theo từng request.
- **Principal**: Danh tính đã xác thực được kiểm tra qua Cedar default-deny.
""",
    }

    # 3. QUICKSTART
    u, _ = _ctx("quickstart")
    pages["quickstart"] = {
        "lead": (
            "Làm theo 4 bước ngắn dưới đây để khởi chạy siêu đồ thị mẫu Walking Skeleton — ghép nối cửa hàng REST OpenAPI (`shop`) với máy chủ MCP (`notes`) trong đúng một truy vấn GraphQL."
        ),
        "learn_bullets": [
            "Kiểm tra cấu hình an toàn bằng `gap config check` và `gap doctor`.",
            "Khởi chạy hai upstream mẫu REST + MCP và tiến trình `gap serve`.",
            "Thực thi câu truy vấn gộp dữ liệu từ cả hai nguồn trong một request duy nhất.",
        ],
        "sections": [
            {
                "id": "prerequisites",
                "h2": "Chuẩn bị Trước khi Chạy",
                "subheadings": [],
                "html": """<ul>
  <li>Binary <code>gap</code> đã biên dịch (từ bản phát hành hoặc chạy <code>./dist/build-reproducible.sh --offline</code> tạo ra <code>./target/dist/gap</code>).</li>
  <li>Python 3.10+ để chạy hai dịch vụ giả lập mẫu trong <code>examples/walking-skeleton/fixtures/</code>.</li>
</ul>""" + callout(
                    "tip",
                    "Bật nhanh ngay trên Trình duyệt",
                    f"<p>Nếu bạn đang mở cổng web cục bộ qua <code>python3 web/serve.py --port 8787</code>, bạn có thể nhấn đúng 1 nút trong trang <a href=\"{esc(u('playground'))}\">Phòng Lab Thực hành</a> để tự bật toàn bộ các tiến trình này.</p>",
                ) + callout(
                    "note",
                    "Tạo mới Workspace từ đầu?",
                    "<p>Bạn có thể khởi tạo nhanh thư mục dự án mẫu 5 tệp bằng lệnh <code>./target/dist/gap init my-gateway</code> rồi chỉnh <code>gap.toml</code>.</p>",
                ),
            },
            {
                "id": "step-by-step",
                "h2": "Hướng dẫn Từng bước",
                "subheadings": [],
                "html": steps_list([
                    (
                        "Kiểm tra hợp lệ tệp cấu hình",
                        """<p>Chạy kiểm tra tĩnh với <code>examples/walking-skeleton/gap.toml</code>. GAP từ chối khởi động ngay nếu thấy trường lạ, bí mật ghi trần không qua biến môi trường hoặc thiếu tệp spec:</p>"""
                        + code_block(
                            "./target/dist/gap config check examples/walking-skeleton/gap.toml\n./target/dist/gap doctor --config examples/walking-skeleton/gap.toml",
                            "terminal",
                            "qs-check",
                            "vi",
                            title="Terminal — Kiểm tra gap.toml",
                        )
                        + code_block(
                            "ok: `examples/walking-skeleton/gap.toml` valid (2 branches)\n[ok] config.parse: parsed examples/walking-skeleton/gap.toml (profile=dev)\n[ok] security.sandbox: OS sandbox operational\n[ok] security.egress_guard: egress SSRF self-test matrix passed\n[warn] models.s1: model cache incomplete offline; degrades safely to baseline (RTE-044)\nsummary: 11 ok, 1 warn, 0 fail, 0 skip",
                            "text",
                            "qs-check-out",
                            "vi",
                            title="Kết quả Mong đợi (Mã thoát 0 · OPS-044)",
                        )
                        + """<ul><li><strong>Lưu ý:</strong> Cả hai lệnh trả về mã thoát <code>0</code> (<code>OPS-044</code>). Khóa API <code>dev-agent</code> được lưu bằng mã băm BLAKE3 trong TOML.</li></ul>""",
                    ),
                    (
                        "Khởi chạy hai máy chủ upstream mẫu",
                        """<p>Bật giả lập REST API trên cổng <code>9101</code> và máy chủ MCP trên cổng <code>9102</code>:</p>"""
                        + code_block(
                            "python3 examples/walking-skeleton/fixtures/rest_fixture.py --port 9101 &\npython3 examples/walking-skeleton/fixtures/mcp_fixture.py --port 9102 &",
                            "terminal",
                            "qs-fixtures",
                            "vi",
                            title="Terminal — Hai Máy chủ Upstream Giả lập",
                        ),
                    ),
                    (
                        "Khởi chạy Cổng GAP Gateway",
                        """<p>Chạy lệnh <code>gap serve</code> trỏ tới cấu hình Walking Skeleton:</p>"""
                        + code_block(
                            "./target/dist/gap serve --config examples/walking-skeleton/gap.toml",
                            "terminal",
                            "qs-serve",
                            "vi",
                            title="Terminal — Khởi chạy Gateway",
                        )
                        + """<ul><li><strong>Kiểm tra sẵn sàng:</strong> <code>GET http://127.0.0.1:8080/readyz</code> trả HTTP <code>200</code> (<code>{"status":"ready","generation":1}</code>) ngay khi thế hệ snapshot <code>1</code> được nạp xong.</li></ul>""",
                    ),
                    (
                        "Gửi truy vấn gộp liên nhánh đầu tiên",
                        """<p>Lấy hồ sơ khách hàng + đơn hàng từ nhánh OpenAPI <code>shop</code> kèm JSON ghi chú từ nhánh MCP <code>notes</code> trong 1 lần gọi HTTP (chú ý <code>customer_notes</code> trả về kiểu vô hướng <code>JSON</code> nên gọi trực tiếp không mở ngoặc con):</p>"""
                        + code_block(
                            'curl -sS -X POST http://127.0.0.1:8080/graphql \\\n  -H "Content-Type: application/json" \\\n  -H "Accept: application/graphql-response+json" \\\n  -H "Authorization: Bearer gap_dev_skeleton_key_01" \\\n  -d \'{"query": "{ customer(id: 1) { id name tier } orders(customer_id: 1) { id total_cents status } customer_notes(customer_id: 1) }"}\' | python3 -m json.tool',
                            "terminal",
                            "qs-curl",
                            "vi",
                            title="Terminal — Truy vấn GraphQL Gộp 2 Nhánh",
                        )
                        + code_block(
                            '{\n  "data": {\n    "customer": { "id": 1, "name": "Ada Lovelace", "tier": "vip" },\n    "orders": [\n      { "id": 101, "total_cents": 4200, "status": "paid" },\n      { "id": 103, "total_cents": 8500, "status": "pending" }\n    ],\n    "customer_notes": {\n      "customer_id": 1,\n      "notes": [{ "note_id": "n-1", "customer_id": 1, "summary": "Prefers consolidated monthly invoice" }]\n    }\n  },\n  "extensions": {\n    "gap": { "cost": { "actual": 7, "planned": 306 }, "generation": 1, "partial": false }\n  }\n}',
                            "json",
                            "qs-curl-out",
                            "vi",
                            title="Kết quả Mong đợi — HTTP 200 JSON",
                        ),
                    ),
                ]),
            },
            {
                "id": "connect-ai-host",
                "h2": "Bước 5: Gắn vào MCP Host hoặc Phát Luồng AG-UI",
                "subheadings": [],
                "html": f"""<p>Thay vì cấu hình từng công cụ REST rời rạc, hãy trỏ AI host vào MCP server 5 tool cố định của GAP qua stdio, hoặc nhận luồng sự kiện AG-UI <code>v1.0</code> qua SSE. Xem chi tiết tích hợp tại <a href="{esc(u('facades-and-hosts'))}">Tích hợp MCP, A2A &amp; AG-UI</a>.</p>"""
                + code_block(
                    '{\n  "mcpServers": {\n    "gap-gateway": {\n      "command": "/path/to/GraphProtocol/target/dist/gap",\n      "args": ["serve", "--config", "examples/walking-skeleton/gap.toml", "--stdio"]\n    }\n  }\n}',
                    "json",
                    "qs-mcp-cfg",
                    "vi",
                    title="mcp_servers.json — Cấu hình Client Stdio",
                )
                + code_block(
                    'curl -sS -N -X POST http://127.0.0.1:8080/ag-ui \\\n  -H "Content-Type: application/json" \\\n  -H "Accept: text/event-stream" \\\n  -H "Authorization: Bearer gap_dev_skeleton_key_01" \\\n  -d \'{"threadId":"t1","runId":"r1","messages":[],"forwardedProps":{"graphql":{"query":"{ customer(id: 1) { id name } customer_notes(customer_id: 1) }"}}}\'',
                    "terminal",
                    "qs-agui-curl",
                    "vi",
                    title="Terminal — Luồng SSE AG-UI v1.0 Trực tiếp",
                ),
            },
        ],
        "markdown": """# Khởi chạy Nhanh (10 phút)

Khởi chạy Walking Skeleton kết hợp nhánh OpenAPI REST (`shop`) và nhánh MCP (`notes`).
1. Kiểm tra cấu hình: `./target/dist/gap config check examples/walking-skeleton/gap.toml`
2. Bật upstream mẫu `rest_fixture.py` (9101) & `mcp_fixture.py` (9102)
3. Chạy `./target/dist/gap serve --config examples/walking-skeleton/gap.toml`
4. Gửi truy vấn gộp tại `POST http://127.0.0.1:8080/graphql`
""",
    }

    # 4. ARCHITECTURE
    u, _ = _ctx("architecture")
    pages["architecture"] = {
        "lead": (
            "GAP đạt được tốc độ hoán đổi snapshot tính bằng micro-giây và khả năng bảo mật tất định nhờ cấm tuyệt đối mã học máy (ML) xuất hiện trên mặt phẳng thực thi nóng (`ARC-002`)."
        ),
        "learn_bullets": [
            "Cách 3 mặt phẳng Thực thi, Khám phá và Kiểm soát phân tách ranh giới cứng.",
            "Vì sao mọi request giữ nguyên một snapshot bất biến qua `ArcSwap` và AVL `OrdMap` thuần Rust.",
            "Tra cứu chi tiết từng thành phần qua Sơ đồ Tương tác 10 Khối.",
        ],
        "sections": [
            {
                "id": "three-planes-diagram",
                "h2": "Phân tách Cứng 3 Mặt phẳng",
                "subheadings": [],
                "html": svg_three_planes("vi")
                + """<p>Ranh giới biên dịch này được kiểm tra bắt buộc ở CI bằng <code>tools/check_layers.py</code>: năm crate thuộc đường thực thi nóng (<code>gap-exec</code>, <code>gap-plan</code>, <code>gap-transport</code>, <code>gap-snapshot</code>, <code>gap-balance</code>) sẽ báo lỗi ngay nếu bất kỳ thư viện ML nào lọt vào cây phụ thuộc.</p>""",
            },
            {
                "id": "interactive-inspector",
                "h2": "Sơ đồ Tương tác 10 Khối Kiến trúc",
                "subheadings": [],
                "html": """<p>Nhấn vào từng khối kiến trúc bên dưới để xem vai trò, mã bất biến bảo vệ và vị trí mã nguồn Rust:</p>
<div id="planes-grid" class="planes-grid"></div>
<div id="arch-inspector" class="inspector-panel" aria-live="polite"></div>""",
            },
            {
                "id": "lock-free-snapshots",
                "h2": "Ghim Snapshot Không khóa & Tự động Rollback",
                "subheadings": [],
                "html": """<ul>
  <li><strong>Không giữ Mutex qua <code>.await</code> (INV-02):</strong> Ngay khi vào gateway, request thực hiện một lệnh đọc nguyên tử <code>ArcSwap::load</code> (~15 ns) và dùng trọn bản <code>Arc&lt;Snapshot&gt;</code> bất biến đó đến khi trả kết quả.</li>
  <li><strong>Cấu trúc AVL <code>OrdMap</code> thuần Rust:</strong> Thay thế crate <code>im</code> (tránh cảnh báo bảo trì <code>RUSTSEC-2026-0097</code>), loại bỏ 100% mã <code>unsafe</code> trong tầng dữ liệu snapshot và giữ workspace sạch <code>0</code> lỗ hổng RUSTSEC.</li>
  <li><strong>Giám sát Rollback tự động 60s (INV-16):</strong> Bộ xuất bản làm nóng sẵn top-100 plan trước khi đổi con trỏ, giải phóng snapshot cũ ở luồng nền và lùi thế hệ tự động nếu tỷ lệ lỗi tăng vọt trên 200 mẫu đầu.</li>
</ul>""",
            },
        ],
        "markdown": """# Kiến trúc 3 Mặt phẳng

GAP tách hệ thống thành 3 mặt phẳng không chồng chéo (cưỡng chế bởi `tools/check_layers.py`):
1. **Mặt phẳng 1 — Thực thi Tất định (Zero-ML, `ARC-002`):** AuthN, Cedar default-deny, `PlanCache` khóa theo `TenantId`, `ArcSwap` + AVL `OrdMap`, `DataLoader` batching, `EgressGuard` SSRF.
2. **Mặt phẳng 2 — Khám phá Lũy tiến:** L0 `__metaGraph`, L1 `__search` (BM25 + HNSW lọc sẵn quyền), L2 `__type`, cùng System-1 tùy chọn fallback an toàn (`INV-17`).
3. **Mặt phẳng 3 — Kiểm soát Zero-Trust:** Propose-Not-Activate, xác minh mẫu `@join`, đóng băng rug-pull `DriftMonitor` (`desc_hash`) và `RollbackWatcher`.
""",
    }

    # 5. PROGRESSIVE DISCLOSURE
    u, _ = _ctx("progressive-disclosure")
    pages["progressive-disclosure"] = {
        "lead": (
            "Thay vì đổ 203.421 tokens schema JSON ngay đầu phiên chat, GAP giữ cố định `tools/list` của MCP ở mức 651 tokens và hé lộ dần thông tin qua 3 tầng (`L0 -> L1 -> L2`)."
        ),
        "learn_bullets": [
            "Cách hoạt động lần lượt của L0 (`__metaGraph`), L1 (`__search`) và L2 (`__type`).",
            "Vì sao bước lọc quyền hạn diễn ra TRƯỚC khi chấm điểm tìm kiếm (`INV-05`).",
            "Kích thước gói tin thực tế ở từng tầng khám phá.",
        ],
        "sections": [
            {
                "id": "three-levels-walkthrough",
                "h2": "Quy trình 3 Tầng Từ Ý định Đến Truy vấn",
                "subheadings": [],
                "html": svg_progressive_levels("vi")
                + steps_list([
                    (
                        "Tầng L0 — Tổng quan Nhánh (`gap_describe{overview: true}` / `__metaGraph`)",
                        "<p>Agent xem danh mục tóm tắt các nhánh hiện có qua MCP <code>POST /mcp</code>, giới hạn &lt;= 24 token mỗi nhánh (đối chiếu cấu trúc <code>MetaGraph</code> trong <code>spec/02-gap-ql.md</code>):</p>"
                        + code_block(
                            '{\n  "jsonrpc": "2.0",\n  "id": 1,\n  "method": "tools/call",\n  "params": {\n    "name": "gap_describe",\n    "arguments": { "overview": true }\n  }\n}',
                            "json",
                            "pd-l0",
                            "vi",
                            title="MCP 2026-07-28 — Gọi Tổng quan L0",
                        )
                        + code_block(
                            "# Hợp đồng MetaGraph trong spec/02-gap-ql.md:\nquery { __metaGraph { generation groupCount tokensEstimate branches { id protocol trust status synopsis operationCount } } }",
                            "graphql",
                            "pd-l0-sdl",
                            "vi",
                            title="Tham chiếu Đặc tả GAP-QL (spec/02-gap-ql.md)",
                        )
                        + """<ul><li><strong>Kết quả trả về gọn nhẹ:</strong> <code>shop (REST, T1, 4 ops)</code>, <code>notes (MCP, T1, 1 op)</code> — dù hệ thống có hàng trăm backend vẫn gói gọn chỉ trong vài trăm tokens.</li></ul>""",
                    ),
                    (
                        "Tầng L1 — Tìm kiếm Lọc sẵn Quyền (`gap_search` / `__search`)",
                        "<p>Agent tìm top-k thao tác phù hợp bằng chỉ mục lai BM25 + vector HNSW. GAP áp dụng Cedar visibility bitset của người gọi <strong>trước khi xếp hạng</strong> (<code>INV-05</code>), nên công cụ không có quyền hoàn toàn biến mất khỏi kết quả:</p>"
                        + code_block(
                            '{\n  "jsonrpc": "2.0",\n  "id": 2,\n  "method": "tools/call",\n  "params": {\n    "name": "gap_search",\n    "arguments": { "query": "customer order notes", "limit": 5 }\n  }\n}',
                            "json",
                            "pd-l1",
                            "vi",
                            title="MCP 2026-07-28 — Tìm kiếm Có Lọc quyền L1",
                        )
                        + code_block(
                            'query { __search(query: "customer order notes", first: 5) { tokensEstimate abstained hits { score kind name path branch sdl } } }',
                            "graphql",
                            "pd-l1-sdl",
                            "vi",
                            title="Cấu trúc SearchResult SDL (spec/02-gap-ql.md)",
                        ),
                    ),
                    (
                        "Tầng L2 — Mở Chi tiết Kiểu Dữ liệu (`gap_describe{types: [...]}` / `__type`)",
                        "<p>Chỉ sau khi chọn đúng thao tác cần gọi, agent mới tải định nghĩa SDL cho đúng các kiểu OpenAPI đã biên dịch trong supergraph (với Walking Skeleton là <code>shop_CustomersResult</code>, <code>shop_OrdersResult</code>):</p>"
                        + code_block(
                            '{\n  "jsonrpc": "2.0",\n  "id": 3,\n  "method": "tools/call",\n  "params": {\n    "name": "gap_describe",\n    "arguments": { "types": ["shop_CustomersResult", "shop_OrdersResult"], "depth": 1 }\n  }\n}',
                            "json",
                            "pd-l2",
                            "vi",
                            title="MCP 2026-07-28 — Mở Kiểu Supergraph L2",
                        )
                        + code_block(
                            'query { __type(name: "shop_CustomersResult") { name fields { name type { name kind } } } }',
                            "graphql",
                            "pd-l2-sdl",
                            "vi",
                            title="Cấu trúc Introspection GAP-QL (spec/02-gap-ql.md)",
                        ),
                    ),
                ]),
            },
            {
                "id": "visibility-before-rank",
                "h2": "Bảo đảm Không Rò rỉ Sự tồn tại (`INV-05`) & Biểu đồ Mở rộng",
                "subheadings": [],
                "html": callout(
                    "important",
                    "Lọc Quyền trước Xếp hạng",
                    "<p>Nếu một tenant hoặc principal bị hạn chế quyền gọi <code>__search</code>, visibility bitset loại bỏ toàn bộ thao tác bị cấm trước khi tính điểm BM25 hay đếm số lượng kết quả.</p>",
                )
                + metric_bar_chart(
                    "pd-scaling",
                    "Thuế Schema MCP tools/list Theo Quy mô N (N = 10 đến 1.000)",
                    "Số token định nghĩa công cụ gửi sẵn tới LLM trước khi có câu hỏi (ADR-021)",
                    [
                        ("<code>GAP 5 Tool Tĩnh (Mọi N)</code>", 651, "651 tok (cố định)", "ok"),
                        ("Direct-MCP (N = 10)", 1904, "1.904 tok", "warn"),
                        ("Direct-MCP (N = 50)", 10066, "10.066 tok", "warn"),
                        ("Direct-MCP (N = 200)", 40586, "40.586 tok", "warn"),
                        ("Direct-MCP (N = 1.000)", 203421, "203.421 tok (nặng hơn 312x)", "warn"),
                    ],
                    203421,
                    "vi",
                )
                + data_table(
                    "Bảng so sánh dung lượng schema tools/list theo quy mô (N = 10 đến 1.000)",
                    ["Quy mô N", "Direct-MCP tools/list", "GAP Static tools/list", "Mức Giảm Token Schema"],
                    [
                        ["N = 10 tools", "1.904 tokens", "<strong>651 tokens</strong>", "65,8%"],
                        ["N = 50 tools", "10.066 tokens", "<strong>651 tokens</strong>", "93,5%"],
                        ["N = 200 tools", "40.586 tokens", "<strong>651 tokens</strong>", "98,4%"],
                        ["N = 1.000 tools", "203.421 tokens", "<strong>651 tokens</strong>", "<strong>99,7% (312x)</strong>"],
                    ],
                ),
            },
        ],
        "markdown": """# Khám phá Lũy tiến (651 Tokens)

GAP giữ MCP `tools/list` cố định ở 651 tokens từ 10 đến 1.000 công cụ:
- **L0 (`__metaGraph`):** Danh mục nhánh rút gọn (<= 24 tokens/nhánh).
- **L1 (`__search`):** Tìm kiếm lai BM25 + HNSW đã lọc quyền trước qua visibility bitset (`INV-05`).
- **L2 (`__type`):** Tải SDL chi tiết theo yêu cầu cho đúng kiểu dữ liệu được chọn.
""",
    }

    # 6. GAP-QL & JOINS
    u, _ = _ctx("gap-ql-and-joins")
    pages["gap-ql-and-joins"] = {
        "lead": (
            "GAP-QL là hồ sơ GraphQL chuẩn mực do `apollo-compiler` phân tích cú pháp, kết hợp cơ chế gom batch `DataLoader` giúp chặn đứng vòng lặp N+1 (`INV-18`)."
        ),
        "learn_bullets": [
            "Vì sao mô hình hóa khám phá bằng field GraphQL chuẩn tương thích mọi công cụ có sẵn.",
            "Cơ chế khử trùng lặp khóa join giới hạn số lời gọi ở `ceil(unique_keys / max_batch)` (`INV-18`).",
            "Bảng đối chứng số token thật trên `N=10..1.000` có và không có Prompt Cache.",
        ],
        "sections": [
            {
                "id": "valid-graphql-profile",
                "h2": "Không Chế Cú pháp Lạ (`GQL-001`)",
                "subheadings": [],
                "html": """<p>Thay vì tạo từ khóa phi chuẩn như <code>traverse { ... }</code>, GAP đặt khám phá và nối nhánh thành các field gốc chuẩn cùng các directive chuẩn trong <code>spec/02-gap-ql.md</code> §5 (<code>@join</code>, <code>@sideEffect</code>, <code>@auth</code>, <code>@pii</code>). Mọi truy vấn qua kiểm tra vi phân đối chiếu trực tiếp với <code>graphql-js 16.13.2</code>.</p>"""
                + code_block(
                    "directive @join(to: String!, on: [JoinKey!]!, cardinality: Cardinality!,\n                strategy: JoinStrategy = FOREACH_BATCH, status: JoinStatus = PROPOSED,\n                evidence: JSON) on FIELD_DEFINITION\ndirective @sideEffect(level: SideEffectLevel!, idempotent: Boolean = false, dryRun: Boolean = false) on FIELD_DEFINITION\ndirective @auth(policy: String, scopes: [String!]) on OBJECT | FIELD_DEFINITION\ndirective @pii(kind: String!) on FIELD_DEFINITION",
                    "graphql",
                    "gql-dirs",
                    "vi",
                    title="spec/02-gap-ql.md §5 — Directives Hợp lệ",
                ),
            },
            {
                "id": "dataloader-batching",
                "h2": "Triệt tiêu Lỗi Thác nước N+1 (`INV-18`)",
                "subheadings": [],
                "html": """<p>Khi một truy vấn lấy 50 khách hàng kèm ghi chú từ nhánh MCP hoặc SQL khác, client phẳng sẽ phát 50 lần gọi nối tiếp. Bộ thực thi GAP rút trích các khóa duy nhất, chia lô đúng giới hạn <code>maxBatch</code> và chỉ gửi tối đa <code>ceil(unique_keys / max_batch)</code> khung <code>BatchQuery</code> — được kiểm tra tự động qua <code>VER-040</code>.</p>"""
                + svg_batch_join("vi")
                + metric_bar_chart(
                    "gapbench-highlight",
                    "Tổng Lượng Token Thực nghiệm GAP-Bench (Join N=1.000 & Code Graph)",
                    "Đối chứng trực tiếp với Direct-MCP trên bài toán join đa nhánh và duyệt AST",
                    [
                        ("Join N=1.000 · Direct-MCP", 814959.8, "814.959,8 tok (4 lượt)", "warn"),
                        ("<code>Join N=1.000 · GAP BatchJoin</code>", 8547.9, "8.547,9 tok (-99,0% · 2 lượt)", "ok"),
                        ("Code Graph (24) · Direct-MCP", 137810.4, "137.810,4 tok (11 lượt)", "warn"),
                        ("<code>Code Graph (24) · GAP AST</code>", 660.4, "660,4 tok (208,7x · 1 lượt)", "accent"),
                    ],
                    814959.8,
                    "vi",
                )
                + data_table(
                    "Số liệu GAP-Bench đo thật (Không Cache vs Prompt Cache 0,10x, N=10..1.000)",
                    ["Kịch bản / Quy mô N", "Direct-MCP (Không Cache)", "GAP (Không Cache)", "Direct-MCP (Có Cache)", "GAP (Có Cache)", "Số lượt (Direct -> GAP)"],
                    [
                        ["<code>cross_branch_join</code> · N=10", "8.891,8", "<strong>8.517,9</strong>", "<strong>2.200,5</strong>", "7.383,7", "4 -> 2"],
                        ["<code>cross_branch_join</code> · N=50", "41.539,8", "<strong>8.547,9</strong>", "<strong>6.740,7</strong>", "7.413,7", "4 -> 2"],
                        ["<code>cross_branch_join</code> · N=200", "163.619,8", "<strong>8.547,9</strong>", "23.717,4", "<strong>7.413,7</strong>", "4 -> 2"],
                        ["<code>cross_branch_join</code> · N=1.000", "814.959,8", "<strong>8.547,9 (-99,0%)</strong>", "114.294,4", "<strong>7.413,7 (-93,5%)</strong>", "4 -> 2"],
                        ["<code>code_graph</code> · 24 symbols", "141.681,3", "<strong>1.187,4 (119,3x)</strong>", "137.810,4", "<strong>660,4 (208,7x)</strong>", "11 -> 1"],
                    ],
                ),
            },
        ],
        "markdown": """# GAP-QL & Gom Lô Join

GAP-QL tương thích 100% `apollo-compiler` và bộ kiểm thử `graphql-js 16.13.2`.
- Ràng buộc `INV-18`: mỗi cạnh `@join` chỉ gửi tối đa `ceil(unique_keys / max_batch)` yêu cầu RHS.
- Ở `N=1.000` cross-branch join: giảm **99,0%** token không cache (**93,5%** có cache) và giảm nửa số lượt LLM (`4 -> 2`).
- Trên `code_graph` (24 symbols): tiết kiệm **119,3x** đến **208,7x** (`11 -> 1` lượt).
""",
    }

    # 7. CONNECTORS OVERVIEW
    u, _ = _ctx("connectors-overview")
    pages["connectors-overview"] = {
        "lead": (
            "Native Graph Connectors (NGC) giúp đưa API REST, cơ sở dữ liệu SQL, gRPC, GraphQL subgraph, CLI và AST mã nguồn vào chung đồ thị mà không phải viết resolver thủ công."
        ),
        "learn_bullets": [
            "Ví dụ trực quan đưa một endpoint OpenAPI REST vào `gap.toml` kèm gom batch `x-gap-in-param`.",
            "So sánh 6 adapter NGC chính thức được đóng gói sẵn trong dự án.",
            "Quy trình kiểm duyệt Propose-Not-Activate trước khi xuất bản snapshot mới.",
        ],
        "sections": [
            {
                "id": "rest-to-gap-walkthrough",
                "h2": "Từ REST JSON đến Nhánh Đồ thị Chỉ với Khai báo",
                "subheadings": [],
                "html": """<p>Với endpoint REST <code>GET /customers/{id}</code> đi kèm endpoint gom danh sách <code>GET /customers?ids=1,2,3</code>, chỉ cần gắn khối <code>"x-gap-in-param"</code> ngay trong đặc tả OpenAPI (trích nguyên văn từ <code>examples/walking-skeleton/shop.openapi.json</code>):</p>"""
                + code_block(
                    '"/customers/{id}": {\n  "get": {\n    "operationId": "customer",\n    "x-gap-in-param": {\n      "path": "/customers",\n      "param": "ids",\n      "key_field": "id",\n      "max_batch_size": 50\n    }\n  }\n}',
                    "json",
                    "ngc-json",
                    "vi",
                    title="examples/walking-skeleton/shop.openapi.json",
                )
                + """<p>Sau đó khai báo nhánh OpenAPI đó trong <code>gap.toml</code>:</p>"""
                + code_block(
                    '[[branches]]\nid = "shop"\nprotocol = "openapi"\ntrust = "T1"\n\n[branches.openapi]\nspec = "shop.openapi.json"\nbase_url = "http://127.0.0.1:9101"',
                    "toml",
                    "ngc-toml",
                    "vi",
                    title="gap.toml — Khai báo Nhánh OpenAPI",
                )
                + """<ul>
  <li><strong><code>id = "shop"</code>:</strong> Định danh nhánh dùng đặt tiền tố kiểu biên dịch (<code>shop_CustomersResult</code>, <code>shop_OrdersResult</code>) và khóa audit.</li>
  <li><strong><code>trust = "T1"</code>:</strong> Nhánh đã được quản trị viên duyệt và ghim mã băm <code>spec_hash</code> / <code>desc_hash</code> chống sửa lén.</li>
</ul>""",
            },
            {
                "id": "six-connectors",
                "h2": "Toàn bộ 6 Adapter NGC Đóng gói Sẵn",
                "subheadings": [],
                "html": data_table(
                    "6 Native Graph Connectors chuẩn hóa hợp đồng protobuf NGC v1",
                    ["Crate Adapter", "Nguồn Dữ liệu", "Cơ chế Batch & Bảo vệ"],
                    [
                        ["<code>gap-ngc-openapi</code>", "OpenAPI 3.0 / REST JSON", "Hỗ trợ gom lô qua <code>x-gap-in-param</code> + ghim IP qua <code>EgressGuard</code>."],
                        ["<code>gap-ngc-sql</code>", "SQLite / PostgreSQL", "Ánh xạ bảng &amp; khóa ngoại thành cạnh đồ thị gom lô <code>IN (...)</code>."],
                        ["<code>gap-ngc-graphql</code>", "GraphQL subgraphs", "Chuyển tiếp chọn lọc kiểu chặt chẽ qua <code>apollo-compiler</code>."],
                        ["<code>gap-ngc-grpc</code>", "Dịch vụ gRPC / Protobuf", "Biên dịch <code>.proto</code> thuần Rust bằng <code>protox</code>, hỗ trợ TLS/mTLS."],
                        ["<code>gap-ngc-cli</code>", "Công cụ dòng lệnh nội bộ", "Cô lập bắt buộc trong OS sandbox Bubblewrap / Seatbelt với biến môi trường giới hạn."],
                        ["<code>gap-ngc-code</code>", "Kho mã nguồn đa ngôn ngữ", "Phân tích AST qua <code>tree-sitter</code> giúp giảm tới 208x token đọc mã nguồn."],
                    ],
                ),
            },
            {
                "id": "propose-not-activate",
                "h2": "Quy trình Duyệt An toàn: Propose-Not-Activate",
                "subheadings": [],
                "html": """<p>Không một schema mới hay cạnh <code>@join</code> nào tự động lọt vào luồng phục vụ khách hàng. Lệnh <code>gap ingest once</code> trích xuất đồ thị trung gian (CISG), kiểm tra thực tế trên dữ liệu mẫu đối với cạnh nối đề xuất (tỷ lệ khớp tập khóa RHS, tỷ lệ null) rồi tạo Proposal chờ người vận hành duyệt:</p>"""
                + code_block(
                    "./target/dist/gap ingest once --config examples/walking-skeleton/gap.toml --tenant default\n./target/dist/gap proposals list\n./target/dist/gap snapshot show --config examples/walking-skeleton/gap.toml\n./target/dist/gap snapshot diff --before gap.toml --after gap.next.toml\n./target/dist/gap proposals approve <proposal-id>",
                    "terminal",
                    "ngc-propose",
                    "vi",
                    title="Terminal — Ingest, Diff & Duyệt Proposal",
                ),
            },
        ],
        "markdown": """# Kết nối Dữ liệu (NGC)

Cả 6 adapter (`OpenAPI`, `SQL`, `GraphQL`, `gRPC`, `CLI`, `Code AST`) đều dùng chung hợp đồng `proto/gap/ngc/v1/ngc.proto` (`Describe`, `BatchQuery`, `Mutate`, `StreamEvents`, `Ping`) và đi qua cổng duyệt `gap proposals approve`.
""",
    }

    return pages
