"""Vietnamese Learn Basics (`graphql.com/learn`) & GAP Academy (`apollographql.com/tutorials`) pages."""
from __future__ import annotations

from .diagrams import (
    svg_batch_join,
    svg_facade_flow,
    svg_progressive_levels,
    svg_query_mirror,
)
from .nav import link_to_page
from .render import (
    callout,
    code_block,
    code_challenge,
    data_table,
    esc,
    going_deeper,
    lesson_footer,
    practice_check,
)


def _ctx(slug: str):
    return lambda target: link_to_page(slug, target, "vi")


def _course_banner(course_id: str, title: str, duration: str, lessons: list[tuple[str, str]]) -> str:
    pills = "".join(
        f'<a class="lesson-step-pill" href="#{esc(sec_id)}" data-lesson-pill="{esc(course_id)}-{idx + 1}">'
        f'<span class="step-num">{idx + 1}</span><span>{esc(label)}</span></a>'
        for idx, (sec_id, label) in enumerate(lessons)
    )
    return f"""<div class="course-banner" data-course-id="{esc(course_id)}">
  <div class="course-banner-top">
    <span><strong>{esc(title)}</strong> · {esc(duration)} · {len(lessons)} bài học</span>
    <span class="pill" data-course-progress="{esc(course_id)}">0 / {len(lessons)} hoàn thành</span>
  </div>
  <nav class="lesson-stepper" aria-label="Danh sách bài học trong khóa">{pills}</nav>
</div>"""


def build_academy_vi() -> dict[str, dict]:
    pages: dict[str, dict] = {}

    # 1. LEARN BASICS — PART 1: GRAPHQL & GAP FOUNDATIONS
    u = _ctx("learn-graphql-and-gap")
    pages["learn-graphql-and-gap"] = {
        "lead": (
            "Mới làm quen với GraphQL hay AI Gateway? Tài liệu nền tảng không dùng biệt ngữ này giúp bạn nắm trực giác từ gốc: vì sao Agent cần một thực đơn thống nhất, Kiểu dữ liệu giống hộp dán nhãn ra sao, và cách truy vấn phản chiếu JSON 1:1."
        ),
        "learn_bullets": [
            "Hình dung Supergraph GAP giống một quầy gọi món trung tâm trước hàng chục bếp REST, SQL, gRPC, MCP và Code.",
            "Đọc thông thạo schema: Kiểu Object (`shop_CustomersResult`), Scalar (`Int`, `String`, `JSON`), Enum, Mảng và Non-Null (`!`).",
            "Bấm chọn trường ngay trong Bộ dựng Truy vấn Tương tác bên dưới để xem GAP-QL khớp JSON đầu ra 1:1.",
        ],
        "sections": [
            {
                "id": "what-is-a-supergraph",
                "h2": "Tư duy Nền: Một Quầy Gọi Món so với Hai Mươi Xe Hàng Rời Rạc",
                "subheadings": [],
                "html": """<p>Hãy tưởng tượng AI Agent là một thực khách bước vào khu ẩm thực lớn có 20 gian bếp riêng: gian bán hồ sơ khách hàng (REST API), gian theo dõi đơn hàng (SQL), và gian giữ ghi chú hỗ trợ (MCP server).</p>
<ul>
  <li><strong>Khi chưa có GAP (Direct-MCP phẳng):</strong> Trước khi kịp gọi món đầu tiên, hệ thống dúi cả 20 cuốn thực đơn dày cộp vào tay Agent — tiêu tốn hàng trăm nghìn tokens JSON lặp đi lặp lại. Tệ hơn, muốn ghép khách hàng với ghi chú, Agent phải tự chạy đi chạy lại từng lượt một.</li>
  <li><strong>Khi có GAP (Supergraph định kiểu chặt chẽ):</strong> GAP đặt <strong>duy nhất một quầy tiếp nhận đã đánh chỉ mục</strong> ở phía trước mọi backend. Thay vì nhồi toàn bộ công thức ngay từ đầu (<code>203.421</code> tokens ở 1.000 tools), Agent chỉ cầm <strong>5 công cụ chuẩn (651 tokens)</strong>, tra đúng món mình cần và gửi một phiếu yêu cầu gộp.</li>
</ul>"""
                + callout(
                    "note",
                    "Hiểu nhanh Supergraph & Branch",
                    "<p><strong>Supergraph</strong> là bản thực đơn hợp nhất do GAP biên dịch sẵn, còn mỗi hệ thống phía sau (<code>shop</code>, <code>notes</code>, <code>billing_sql</code>) chính là một <strong>Branch (nhánh)</strong> nằm trong thực đơn đó.</p>",
                ),
            },
            {
                "id": "types-scalars-nullability",
                "h2": "Kiểu Dữ liệu Như Chiếc Hộp Dán Nhãn (`!` Là Cam kết Không Rỗng)",
                "subheadings": [],
                "html": """<p>Schema trong GraphQL và GAP thực chất chỉ là bản hợp đồng mô tả rõ có những chiếc hộp dán nhãn nào và bên trong mỗi hộp chứa kiểu dữ liệu gì:</p>"""
                + data_table(
                    "Các thành phần cấu tạo schema minh họa qua Walking Skeleton",
                    ["Thành phần Schema", "Ví dụ Minh họa", "Ý nghĩa Dễ hiểu"],
                    [
                        ["<strong>Object Type (Đối tượng)</strong>", "<code>type shop_CustomersResult &#123; id: Int! name: String! tier: ... &#125;</code>", "Một bản ghi có cấu trúc đại diện cho thực thể (ví dụ dòng khách hàng lấy từ OpenAPI)."],
                        ["<strong>Scalar (Giá trị lá)</strong>", "<code>Int</code>, <code>String</code>, <code>Boolean</code>, <code>JSON</code>", "Giá trị nguyên thủy nơi câu truy vấn dừng lại — không bao giờ mở ngoặc <code>&#123; ... &#125;</code> chọn tiếp bên trong scalar."],
                        ["<strong>Non-Null (<code>!</code>)</strong>", "<code>id: Int!</code> so với <code>name: String</code>", "Dấu chấm than cam kết GAP không bao giờ trả về <code>null</code> cho trường này khi truy vấn thành công."],
                        ["<strong>List (Mảng <code>[...]</code>)</strong>", "<code>orders(customer_id: Int): [shop_OrdersResult!]</code>", "Trả về một danh sách chứa không, một hoặc nhiều phần tử cùng kiểu."],
                        ["<strong>Enum (Tập chọn cố định)</strong>", "<code>shop_CustomersResultTier</code> (<code>vip</code>, <code>standard</code>)", "Danh sách hằng số định sẵn được kiểm tra hợp lệ trước khi chạm tới backend."],
                    ],
                )
                + going_deeper(
                    "Tìm hiểu sâu hơn: Vì sao `customer_notes(customer_id: 1)` KHÔNG mở ngoặc nhọn phía sau?",
                    "<p>Trong <code>examples/walking-skeleton</code>, hai endpoint REST OpenAPI (<code>shop_CustomersResult</code>, <code>shop_OrdersResult</code>) được biên dịch thành Object Type định kiểu rõ các trường nên bạn chọn <code>&#123; id name tier &#125;</code>. Ngược lại, công cụ MCP ngoài <code>customer_notes(customer_id: Int!): JSON</code> trả về kiểu scalar lá <code>JSON</code> — nên viết <code>customer_notes(customer_id: 1) &#123; notes &#125;</code> sẽ bị lỗi cú pháp GraphQL ngay! Hãy nhớ: scalar là điểm lá, không mở ngoặc chọn con.</p>",
                ),
            },
            {
                "id": "query-mirrors-json",
                "h2": "Truy vấn Phản chiếu JSON 1:1 (Bộ dựng Truy vấn Tương tác)",
                "subheadings": [],
                "html": """<p>Khác với REST URL nơi server trả về cố định toàn bộ 40 trường dù bạn chỉ cần tên, câu truy vấn GAP-QL đóng vai trò như một khuôn mẫu JSON: <strong>bạn hỏi trường nào thì kết quả trả về đúng cấu trúc y hệt trường đó, không thừa một byte</strong>.</p>"""
                + svg_query_mirror("vi")
                + f"""<div class="query-builder" id="query-builder-widget">
  <div class="qb-header">
    <h3>Tích chọn Trường để Dựng Câu Truy vấn Walking Skeleton Sống</h3>
    <span class="pill" id="qb-stats-pill">1 request GAP · thay vì 3 lượt Direct</span>
  </div>
  <div class="qb-controls">
    <span class="qb-group-title">Chọn trường muốn lấy:</span>
    <div class="qb-check-row">
      <input type="checkbox" id="qb-id" checked disabled />
      <label for="qb-id"><code>customer.id</code> (luôn kèm định danh)</label>
    </div>
    <div class="qb-check-row">
      <input type="checkbox" id="qb-name" checked />
      <label for="qb-name"><code>customer.name</code></label>
    </div>
    <div class="qb-check-row">
      <input type="checkbox" id="qb-tier" checked />
      <label for="qb-tier"><code>customer.tier</code></label>
    </div>
    <div class="qb-check-row">
      <input type="checkbox" id="qb-orders" />
      <label for="qb-orders"><code>orders(customer_id: 1)</code> (nhánh REST shop)</label>
    </div>
    <div class="qb-check-row">
      <input type="checkbox" id="qb-notes" checked />
      <label for="qb-notes"><code>customer_notes(customer_id: 1)</code> (scalar JSON từ MCP)</label>
    </div>
  </div>
  <div class="qb-panes">
    <figure class="code-figure">
      <div class="code-toolbar">
        <div class="code-meta">
          <span class="code-lang">graphql</span>
          <span class="code-title">Câu Truy vấn GAP-QL Tạo Tự động</span>
        </div>
        <span class="copy">
          <button type="button" class="copy-button" command="--copy" commandfor="qb-graphql-code" aria-label="Sao chép truy vấn GAP-QL vừa dựng">
            <span class="icon" aria-hidden="true"></span>
          </button>
          <span class="tip" role="status"></span>
        </span>
      </div>
      <pre role="region" aria-label="Truy vấn GAP-QL vừa dựng" tabindex="0"><code id="qb-graphql-code" class="language-graphql">query &#123;
  customer(id: 1) &#123;
    id
    name
    tier
  &#125;
  customer_notes(customer_id: 1)
&#125;</code></pre>
    </figure>
    <figure class="code-figure">
      <div class="code-toolbar">
        <div class="code-meta">
          <span class="code-lang">json</span>
          <span class="code-title">JSON Trả về Tương ứng 1:1</span>
        </div>
        <span class="copy">
          <button type="button" class="copy-button" command="--copy" commandfor="qb-json-code" aria-label="Sao chép kết quả JSON phản chiếu">
            <span class="icon" aria-hidden="true"></span>
          </button>
          <span class="tip" role="status"></span>
        </span>
      </div>
      <pre role="region" aria-label="Kết quả JSON phản chiếu" tabindex="0"><code id="qb-json-code" class="language-json">&#123;
  "data": &#123;
    "customer": &#123;
      "id": 1,
      "name": "Ada Lovelace",
      "tier": "vip"
    &#125;,
    "customer_notes": &#123;
      "customer_id": 1,
      "notes": [
        &#123;
          "note_id": "n-1",
          "summary": "Prefers consolidated monthly invoice"
        &#125;
      ]
    &#125;
  &#125;
&#125;</code></pre>
    </figure>
  </div>
  <p class="section-lead" style="margin-top:0.65rem;">Tiếp tục sang <a href="{esc(u('learn-queries-mutations-joins'))}">Tham số, Mutation &amp; Nối nhánh</a> hoặc bước vào thực chiến ngay tại <a href="{esc(u('academy-hub'))}">Học viện GAP</a>.</p>
</div>""",
            },
        ],
        "markdown": """# Nhập môn GraphQL & GAP

- **Supergraph & Branch**: GAP đóng vai trò một quầy thực đơn định kiểu thống nhất trước REST, SQL, GraphQL, gRPC, CLI, Code AST và MCP.
- **Kiểu dữ liệu & Non-Null**: Kiểu Object mở `{ ... }`; kiểu scalar lá (`Int`, `String`, `JSON`) dừng tại chỗ; dấu `!` đảm bảo khác null.
- **Phản chiếu 1:1**: Hình dáng truy vấn khớp đúng 1:1 cấu trúc JSON nhận về trong một request duy nhất.
""",
    }

    # 2. LEARN BASICS — PART 2: ARGUMENTS, MUTATIONS & JOINS
    u = _ctx("learn-queries-mutations-joins")
    pages["learn-queries-mutations-joins"] = {
        "lead": (
            "Làm chủ việc truyền tham số qua variables, cập nhật dữ liệu an toàn với `@sideEffect` mutation, nối nhiều nhánh bằng gom lô và chuyển đổi mượt tư duy từ REST / Direct-MCP sang GAP."
        ),
        "learn_bullets": [
            "Truyền GraphQL `variables` chuẩn định kiểu thay cho nối chuỗi để tận dụng tối đa `PlanCache`.",
            "Phân biệt rạch ròi `query` (chỉ đọc) với `mutation` `@sideEffect` có chế độ thử nghiệm `dry_run` và Cedar duyệt 2 người.",
            "Đối chiếu từng thói quen lập trình REST và Direct-MCP sang cơ chế tương ứng trong GAP.",
        ],
        "sections": [
            {
                "id": "arguments-and-variables",
                "h2": "Tham số & Biến Định kiểu (`PlanCache` Reuse)",
                "subheadings": [],
                "html": """<p>Tương tự tham số query trong REST URL, các trường GraphQL nhận đối số rõ kiểu như <code>customer(id: 1)</code> hay <code>orders(customer_id: 1)</code>. Đừng bao giờ nối chuỗi input từ người dùng hoặc LLM trực tiếp vào nội dung truy vấn — hãy khai báo biến tham số (<code>$cid: Int!</code>):</p>"""
                + code_block(
                    'query GetCustomerDossier($cid: Int!) {\n  customer(id: $cid) { id name tier }\n  orders(customer_id: $cid) { id status total_cents }\n  customer_notes(customer_id: $cid)\n}\n\n# JSON biến tách riêng:\n# { "cid": 1 }',
                    "graphql",
                    "lrn-vars",
                    "vi",
                    title="Truy vấn Tham số hóa + Variables JSON",
                )
                + """<ul>
  <li><strong>Không sợ chèn mã cú pháp:</strong> Biến tách riêng hoàn toàn khỏi cây cú pháp GraphQL.</li>
  <li><strong>Tái sử dụng Kế hoạch Nóng:</strong> Nhờ chuỗi query giữ nguyên cho mọi khách hàng, GAP dùng lại kế hoạch thực thi đã biên dịch trong <code>PlanCache</code> gắn khóa <code>(TenantId, snapshot_generation, query_hash)</code> (<code>INV-09</code>).</li>
</ul>""",
            },
            {
                "id": "mutations-and-side-effects",
                "h2": "Mutation & Hàng rào An toàn `@sideEffect`",
                "subheadings": [],
                "html": """<p>Trong khi <code>query</code> chỉ đọc dữ liệu trên snapshot bất biến, mọi thao tác làm thay đổi trạng thái bắt buộc đi qua <code>mutation</code> (hoặc tool MCP <code>gap_mutate</code>) có gắn chỉ thị <code>@sideEffect</code>:</p>"""
                + data_table(
                    "Phân cấp @sideEffect trong spec/02-gap-ql.md §5 và phản ứng của Cedar",
                    ["Cấp độ SideEffectLevel", "Bản chất Tác động", "Ứng xử Mặc định của Cedar Policy"],
                    [
                        ["<code>NONE</code>", "Thao tác chỉ đọc, không sửa đổi trạng thái.", "Cho phép đối với principal có quyền <code>Read</code> hợp lệ."],
                        ["<code>REVERSIBLE</code>", "Ghi nháp hoặc thao tác có thể hoàn tác.", "Cho phép khi có quyền <code>Mutate</code>; mọi lệnh ghi thẳng vào chuỗi băm BLAKE3."],
                        ["<code>IRREVERSIBLE</code> / <code>UNKNOWN</code>", "Chuyển tiền, xóa sổ cái hoặc công cụ chưa phân loại rủi ro.", "Từ chối cứng trừ khi thỏa mãn <code>context.approved &amp;&amp; context.approverIsDifferent</code>."],
                    ],
                )
                + code_block(
                    '{\n  "jsonrpc": "2.0",\n  "id": 10,\n  "method": "tools/call",\n  "params": {\n    "name": "gap_mutate",\n    "arguments": {\n      "query": "mutation { issue_refund(order_id: 101) { ok } }",\n      "dry_run": true\n    }\n  }\n}',
                    "json",
                    "lrn-dryrun",
                    "vi",
                    title="MCP 2026-07-28 — Chạy thử an toàn với dry_run qua gap_mutate",
                ),
            },
            {
                "id": "joins-and-mental-model-map",
                "h2": "Bảng Chuyển đổi Tư duy cho Kỹ sư REST & Direct-MCP",
                "subheadings": [],
                "html": """<p>Bạn không cần viết lại các hệ thống REST API hay máy chủ MCP đang chạy ổn định — chỉ cần giữ nguyên chúng và ánh xạ vào <code>gap.toml</code> qua các NGC Connector.</p>"""
                + data_table(
                    "Đối chiếu từng thói quen REST / Direct-MCP sang mô hình GAP",
                    ["Cách làm Cũ (REST / Direct-MCP)", "Thành phần Tương ứng trong GAP", "Lợi ích Kỹ thuật Cốt lõi"],
                    [
                        ["Nhiều URL dịch vụ lẻ / nhiều file cấu hình MCP", "Một cổng gateway duy nhất (<code>/graphql</code>, <code>/mcp</code>, <code>/ag-ui</code>)", "Dùng chung tầng xác thực, truyền tải TLS 1.3 QUIC và ranh giới snapshot."],
                        ["Tải <code>tools/list</code> chứa 500 JSON Schema", "5 tool tĩnh (<code>651 tokens</code>) + <code>gap_describe</code> / <code>gap_search</code>", "Giảm tới 99% chi phí schema đồng thời giấu kín tool không có quyền (<code>INV-05</code>)."],
                        ["Tự viết vòng lặp <code>for id in ids: fetch(...)</code>", "<code>@join</code> + tiện ích OpenAPI <code>x-gap-in-param</code>", "Khử trùng lặp khóa ngoại thành <code>ceil(K/max_batch)</code> request (<code>INV-18</code>)."],
                        ["Viết hàm kiểm tra quyền rải rác hoặc tin lời nhắc LLM", "Luật Cedar thuần hàm tất định (<code>policies/default.cedar</code>)", "Mặc định từ chối, tách biệt 2 người duyệt cho thao tác không thể hoàn tác."],
                        ["Tin tưởng mù quáng chuỗi mô tả từ upstream", "Ghim băm JCS BLAKE3 <code>desc_hash</code> + bọc <code>&lt;gap:untrusted&gt;</code>", "Đóng băng tự động trạng thái <code>BranchStatus::Drift</code> nếu phát hiện rug-pull."],
                    ],
                )
                + going_deeper(
                    "Tìm hiểu sâu hơn: Vì sao GAP dùng cú pháp GraphQL chuẩn thay vì tự sáng chế ngôn ngữ mới?",
                    f"""<p>Theo chuẩn <code>GQL-001</code> trong <code>spec/02-gap-ql.md</code>, việc chế thêm từ khóa lạ như <code>traverse</code> sẽ làm hỏng mọi trình kiểm tra cú pháp, IDE plugin và thư viện phân tích có sẵn. Giữ 100% GraphQL chuẩn cho phép GAP phân tích bằng bộ biên dịch <code>apollo-compiler</code> bằng Rust, vượt qua 136/136 bài kiểm thử đối chiếu với <code>graphql-js 16.13.2</code> và biểu diễn toàn bộ nối nhánh qua directive chuẩn <code>@join</code>, <code>@sideEffect</code>. Sẵn sàng viết lệnh? Bắt đầu ngay tại <a href="{esc(u('academy-hub'))}">Học viện GAP</a>.</p>""",
                ),
            },
        ],
        "markdown": """# Tham số, Mutation & Nối nhánh

- Luôn truyền biến `variables` (`$cid: Int!`) tách khỏi câu query để `PlanCache` tái sử dụng theo khóa `(TenantId, generation, query_hash)`.
- Mọi mutation đi qua phân loại `@sideEffect` và `dry_run`; mức `IRREVERSIBLE` bắt buộc 2 người duyệt khác nhau theo luật Cedar.
- REST OpenAPI và MCP server hiện hữu được giữ nguyên hoàn toàn và gắn vào supergraph dưới dạng Branch khai báo.
""",
    }

    # 3. GAP ACADEMY HUB (ODYSSEY COURSE CATALOG)
    u = _ctx("academy-hub")
    pages["academy-hub"] = {
        "lead": (
            "Chào mừng tới Học viện GAP — chuỗi khóa học thực chiến nhiều bài giảng lấy cảm hứng từ Apollo Odyssey. Kết hợp nền tảng giao thức với câu lệnh thật, bài tập điền mã, kiểm tra nhanh và lưu tự động tiến độ học tập trên 16 bài học."
        ),
        "learn_bullets": [
            "Đi theo lộ trình 4 khóa học theo dự án (`16` bài học) từ siêu đồ thị đầu tiên tới triển khai HTTP/3 Zero-Trust.",
            "Tự kiểm chứng kiến thức ngay trong từng bài với hộp Thử thách Điền mã và trắc nghiệm tức thì.",
            "Lưu đồng bộ số bài đã hoàn thành qua `localStorage` cho cả phiên bản Tiếng Việt lẫn Tiếng Anh.",
        ],
        "sections": [
            {
                "id": "academy-progress",
                "h2": "Tiến độ Học viện của Bạn",
                "subheadings": [],
                "html": """<div class="academy-progress-banner" id="academy-hub-progress">
  <div class="academy-prog-top">
    <div>
      <strong>Tổng tiến độ Học viện:</strong> Đã hoàn thành <span id="academy-total-count">0 / 16</span> bài học
    </div>
    <button type="button" id="academy-reset-btn" class="btn btn-sm">Đặt lại Tiến độ</button>
  </div>
  <div class="progress-track" aria-hidden="true">
    <div id="academy-total-fill" class="progress-fill"></div>
  </div>
  <p class="section-lead" style="margin:0.45rem 0 0 0;">Chưa quen schema hay cách truy vấn phản chiếu JSON? Hãy lướt qua <a href="../learn/graphql-and-gap-basics.html">Nhập môn GraphQL &amp; GAP</a> trước khi vào Khóa 1.</p>
</div>""",
            },
            {
                "id": "course-catalog",
                "h2": "Chọn Khóa học Thực chiến (4 Chặng · 16 Bài học)",
                "subheadings": [],
                "html": f"""<div class="course-grid">
  <article class="course-card">
    <div class="course-meta">
      <span class="path-tag">Cơ bản · 20 phút · 4 bài</span>
      <span class="pill" data-course-progress="liftoff">0 / 4 bài</span>
    </div>
    <h3>Khóa 1: Lift-Off — Siêu đồ thị Đầu tiên</h3>
    <p>Khởi tạo dự án bằng <code>gap init</code>, gắn nhánh OpenAPI <code>shop</code> và MCP <code>notes</code>, kiểm tra qua <code>gap doctor</code> và thực thi câu truy vấn gộp đầu tiên.</p>
    <a class="btn btn-primary" href="{esc(u('course-liftoff'))}">Bắt đầu Khóa 1 &rarr;</a>
  </article>

  <article class="course-card">
    <div class="course-meta">
      <span class="path-tag">AI Agent · 20 phút · 4 bài</span>
      <span class="pill" data-course-progress="mcp">0 / 4 bài</span>
    </div>
    <h3>Khóa 2: MCP, AG-UI &amp; Trợ lý AI</h3>
    <p>Thay thế khối schema phình to <code>203.421</code> token bằng <code>651</code> token khám phá lũy tiến (<code>L0 -&gt; L1 -&gt; L2</code>), kết nối Claude/Cursor <code>--stdio</code> và phát SSE AG-UI <code>v1.0</code>.</p>
    <a class="btn btn-primary" href="{esc(u('course-mcp-agents'))}">Bắt đầu Khóa 2 &rarr;</a>
  </article>

  <article class="course-card">
    <div class="course-meta">
      <span class="path-tag">Liên kết · 25 phút · 4 bài</span>
      <span class="pill" data-course-progress="batch">0 / 4 bài</span>
    </div>
    <h3>Khóa 3: REST, SQL &amp; Gom lô N+1</h3>
    <p>Dập tắt vòng lặp thác nước N+1 bằng tiện ích OpenAPI <code>x-gap-in-param</code> (<code>INV-18</code>), gắn SQL &amp; <code>tree-sitter</code> Code AST và duyệt Proposal an toàn.</p>
    <a class="btn btn-primary" href="{esc(u('course-connectors-batching'))}">Bắt đầu Khóa 3 &rarr;</a>
  </article>

  <article class="course-card">
    <div class="course-meta">
      <span class="path-tag">Vận hành · 25 phút · 4 bài</span>
      <span class="pill" data-course-progress="sec">0 / 4 bài</span>
    </div>
    <h3>Khóa 4: Zero-Trust, Cedar &amp; HTTP/3</h3>
    <p>Gia cố gateway với cô lập <code>T0/T1/T2</code>, luật Cedar duyệt 2 người, <code>DriftMonitor</code> chống rug-pull, <code>EgressGuard</code> chặn SSRF, HTTP/3 QUIC và kiểm chứng chuỗi BLAKE3.</p>
    <a class="btn btn-primary" href="{esc(u('course-enterprise-zero-trust'))}">Bắt đầu Khóa 4 &rarr;</a>
  </article>
</div>""",
            },
        ],
        "markdown": """# Học viện GAP — Khóa học Thực chiến

4 khóa học tương tác từng bước (tổng 16 bài học):
1. [Khóa 1: Lift-Off — Siêu đồ thị Đầu tiên](liftoff-first-supergraph.html)
2. [Khóa 2: MCP, AG-UI & Trợ lý AI](mcp-and-ai-assistants.html)
3. [Khóa 3: REST, SQL & Gom lô N+1](connectors-and-batching.html)
4. [Khóa 4: Zero-Trust, Cedar & HTTP/3](enterprise-zero-trust.html)
""",
    }

    # 4. COURSE 1: LIFT-OFF — FIRST SUPERGRAPH
    pages["course-liftoff"] = {
        "lead": (
            "Khóa 1 đưa bạn từ màn hình terminal trống tới một siêu đồ thị chạy thật gồm hai nhánh: cửa hàng REST OpenAPI (shop) và máy chủ MCP (notes) sau cổng `/graphql` và `/mcp`."
        ),
        "learn_bullets": [
            "Khởi tạo bộ khung dự án mẫu bằng `./target/dist/gap init` và hiểu cơ chế băm khóa BLAKE3.",
            "Chạy kiểm định cấu hình chặt fail-closed (`gap config check` và `gap doctor`) trước khi mở cổng.",
            "Truy vấn gộp Object OpenAPI và scalar `JSON` MCP mà không dính lỗi `GAP_UNKNOWN_FIELD`.",
        ],
        "sections": [
            {
                "id": "liftoff-lesson-1",
                "h2": "Bài 1: Khởi tạo Workspace Mới (`gap init`)",
                "subheadings": [],
                "html": _course_banner(
                    "liftoff",
                    "Khóa 1: Lift-Off",
                    "20 phút",
                    [
                        ("liftoff-lesson-1", "Khởi tạo"),
                        ("liftoff-lesson-2", "Config & Doctor"),
                        ("liftoff-lesson-3", "Bật & Kiểm tra"),
                        ("liftoff-lesson-4", "Truy vấn Gộp"),
                    ],
                )
                + """<p>Trong thực tế triển khai, bạn không cần tự gõ cấu hình TOML từ đầu. Lệnh <code>gap init</code> tạo ngay bộ khung 5 tệp tiêu chuẩn gồm <code>gap.toml</code>, đặc tả OpenAPI mẫu, kịch bản fixture giả lập và mã khóa Bearer phát triển băm sẵn bằng <strong>BLAKE3-256</strong>:</p>"""
                + code_block(
                    "./target/dist/gap init my-shop-gateway\nls -la my-shop-gateway",
                    "terminal",
                    "c1-l1-init",
                    "vi",
                    title="Terminal — Khởi tạo Cổng Gateway Mẫu",
                )
                + callout(
                    "note",
                    "Không bao giờ lưu Secret gốc trong Cấu hình",
                    "<p>Tệp <code>gap.toml</code> chỉ chứa <code>key_hash = \"8a7d...\"</code> (ứng với khóa dev <code>gap_dev_skeleton_key_01</code> trong Walking Skeleton). Cổng GAP kiểm chứng header <code>Authorization: Bearer ...</code> bằng phép so khớp BLAKE3 hằng thời gian và tự động che mọi bí mật (<code>INV-15</code>).</p>",
                )
                + lesson_footer(
                    "liftoff-1",
                    [
                        "`gap init <dir>` sinh bộ dự án mẫu đầy đủ chỉ trong 1 giây.",
                        "`gap.toml` chỉ lưu giá trị băm BLAKE3 (`key_hash`), tuyệt đối không chứa token thô ở bản production.",
                    ],
                    "vi",
                ),
            },
            {
                "id": "liftoff-lesson-2",
                "h2": "Bài 2: Kết nối Nhánh OpenAPI & MCP (`gap config check` & `doctor`)",
                "subheadings": [],
                "html": """<p>Hãy mở <code>examples/walking-skeleton/gap.toml</code>. Bạn sẽ thấy hai khối <code>[[branches]]</code>: <code>id = "shop"</code> (OpenAPI 3.0 chạy ở cổng <code>9101</code>) và <code>id = "notes"</code> (MCP HTTP tại cổng <code>9102/mcp</code>). Kiểm tra cú pháp cấu hình và điều kiện môi trường trước khi chạy:</p>"""
                + code_block(
                    "./target/dist/gap config check examples/walking-skeleton/gap.toml\n./target/dist/gap doctor --config examples/walking-skeleton/gap.toml",
                    "terminal",
                    "c1-l2-doc",
                    "vi",
                    title="Terminal — Kiểm tra Cấu hình & Chẩn đoán Hệ thống",
                )
                + code_block(
                    "ok: examples/walking-skeleton/gap.toml (profile=dev, branches=2, api_keys=1)\n[ok] config.parse: parsed examples/walking-skeleton/gap.toml (profile=dev)\n[ok] config.files: all referenced branch files exist and are readable\n[ok] server.listen: valid socket address 127.0.0.1:8080\n[ok] auth.api_keys: 1 API key(s) configured",
                    "text",
                    "c1-l2-out",
                    "vi",
                    title="Kết quả Chẩn đoán Chuẩn",
                )
                + lesson_footer(
                    "liftoff-2",
                    [
                        "`gap config check` từ chối khởi động ngay nếu phát hiện cấu hình, cổng mạng hoặc đường dẫn tệp sai.",
                        "`gap doctor` kiểm tra toàn diện socket, quyền đọc nhánh, API key và trạng thái sandbox hệ điều hành.",
                    ],
                    "vi",
                ),
            },
            {
                "id": "liftoff-lesson-3",
                "h2": "Bài 3: Khởi động Dịch vụ Upstream & Kiểm tra `/readyz`",
                "subheadings": [],
                "html": """<p>Chạy hai tiến trình giả lập upstream bằng Python trước, rồi bật <code>gap serve</code> và thăm dò trạng thái <code>/healthz</code> cùng <code>/readyz</code>:</p>"""
                + code_block(
                    "python3 examples/walking-skeleton/fixtures/rest_fixture.py --port 9101 &\npython3 examples/walking-skeleton/fixtures/mcp_fixture.py --port 9102 &\n./target/dist/gap serve --config examples/walking-skeleton/gap.toml &\ncurl -sS http://127.0.0.1:8080/readyz",
                    "terminal",
                    "c1-l3-boot",
                    "vi",
                    title="Terminal — Chạy Upstream, Gateway & Kiểm tra /readyz",
                )
                + """<p>Điểm cuối <code>/readyz</code> chỉ trả về <code>&#123;"status":"ready","generation":1&#125;</code> sau khi Snapshot thế hệ 1 biên dịch hoàn tất và nạp nguyên tử vào con trỏ không khóa <code>ArcSwap</code>.</p>"""
                + lesson_footer(
                    "liftoff-3",
                    [
                        "`/healthz` báo tiến trình còn sống; `/readyz` chỉ xanh khi Snapshot generation >= 1 đã biên dịch xong.",
                        "Bạn cũng có thể bấm nút Bật Lab Thực Hành trong trang Phòng Lab để khởi chạy tự động cả 3 tiến trình.",
                    ],
                    "vi",
                ),
            },
            {
                "id": "liftoff-lesson-4",
                "h2": "Bài 4: Truy vấn Gộp Liên Nhánh Đầu Tiên & Thực hành Điền mã",
                "subheadings": [],
                "html": """<p>Bây giờ hãy gửi một yêu cầu <code>POST /graphql</code> duy nhất vừa lấy khách hàng số <code>1</code> và các đơn hàng từ nhánh REST <code>shop</code>, vừa đọc ghi chú từ nhánh MCP <code>notes</code>:</p>"""
                + code_block(
                    'curl -sS http://127.0.0.1:8080/graphql \\\n  -H "Content-Type: application/json" \\\n  -H "Authorization: Bearer gap_dev_skeleton_key_01" \\\n  -d \'{"query": "{ customer(id: 1) { id name tier } orders(customer_id: 1) { id status total_cents } customer_notes(customer_id: 1) }"}\'',
                    "terminal",
                    "c1-l4-curl",
                    "vi",
                    title="Terminal — Gộp Dữ liệu REST shop + MCP notes Trong 1 Lượt",
                )
                + code_challenge(
                    "c1-leaf",
                    "Hoàn thiện Tên Tham số Truy vấn MCP",
                    "<p>Điền đúng tên tham số vào <code>customer_notes(____: 1)</code> để khớp hợp đồng của nhánh MCP <code>notes</code> (lưu ý: <code>customer_notes</code> trả về scalar lá <code>JSON</code>):</p>",
                    "query {\n  customer(id: 1) { id name tier }\n  customer_notes(____: 1)\n}",
                    ["customer_id"],
                    ["customer_id", "id", "cust_id"],
                    "Trong examples/walking-skeleton/fixtures/mcp_fixture.py, tool MCP nhận đối số customer_id và trả về giá trị scalar JSON (không mở ngoặc chọn trường con).",
                    "vi",
                )
                + practice_check(
                    "c1-q1",
                    "Vì sao trình xác thực GAP từ chối câu truy vấn `customer_notes(customer_id: 1) { notes }`?",
                    [
                        "Vì `customer_notes` trả về kiểu scalar lá `JSON`, mà scalar trong GraphQL không thể mở ngoặc `{ ... }` để chọn trường con.",
                        "Vì GAP không cho phép gọi nhánh MCP từ cổng `POST /graphql`.",
                        "Vì chỉ thao tác `mutation` mới truy vấn được nhánh MCP.",
                    ],
                    0,
                    "Kiểu Scalar là điểm dừng cuối cùng của cây chọn trường trong GraphQL. Chỉ Object/Interface/Union mới mở ngoặc nhọn { ... }.",
                    "vi",
                )
                + lesson_footer(
                    "liftoff-4",
                    [
                        "Một request `POST /graphql` duy nhất chạy đồng thời sang cả nhánh REST và MCP.",
                        "Chuyển tiếp sang Khóa 2: MCP, AG-UI & Trợ lý AI ở bài kế tiếp.",
                    ],
                    "vi",
                ),
            },
        ],
        "markdown": """# Khóa 1: Lift-Off — Siêu đồ thị Đầu tiên

1. `gap init` khởi tạo dự án mẫu và băm khóa API bằng BLAKE3 (`INV-15`).
2. `gap config check` và `gap doctor` kiểm tra chặt trước khi mở cổng.
3. `/readyz` chỉ trả lời sẵn sàng khi Snapshot thế hệ 1 biên dịch thành công.
4. Một câu truy vấn lấy song song Object OpenAPI (`{ id name }`) và scalar MCP `JSON` (`customer_notes(customer_id: 1)`).
""",
    }

    # 5. COURSE 2: MCP, AG-UI & AI ASSISTANTS
    pages["course-mcp-agents"] = {
        "lead": (
            "Khóa 2 hướng dẫn cách kết nối trợ lý AI với hàng trăm dịch vụ mà không làm tràn bộ nhớ ngữ cảnh LLM — dùng cơ chế khám phá lũy tiến MCP 2026-07-28 (`L0 -> L1 -> L2`) và luồng sự kiện sống AG-UI v1.0 SSE."
        ),
        "learn_bullets": [
            "Đối chiếu `203.421` token tải phẳng trong `tools/list` với đúng 5 tool tĩnh `651` token của GAP.",
            "Thực hành lần lượt `gap_describe` (`L0`/`L2`), tìm kiếm lọc quyền `gap_search` (`L1`, `INV-05`) và `gap_query`.",
            "Cấu hình AI host chạy `--stdio` (Claude Code, Cursor, Antigravity) và theo dõi SSE AG-UI `v1.0` tại `/ag-ui`.",
        ],
        "sections": [
            {
                "id": "mcp-lesson-1",
                "h2": "Bài 1: Chặn đứng Lạm phát 203.421 Token (`tools/list` Cố định 651 Tokens)",
                "subheadings": [],
                "html": _course_banner(
                    "mcp",
                    "Khóa 2: MCP & Trợ lý AI",
                    "20 phút",
                    [
                        ("mcp-lesson-1", "Bề mặt 651 Token"),
                        ("mcp-lesson-2", "L0 -> L1 -> L2"),
                        ("mcp-lesson-3", "Host --stdio"),
                        ("mcp-lesson-4", "Luồng AG-UI SSE"),
                    ],
                )
                + """<p>Trong tích hợp MCP thông thường, nối 100 dịch vụ đồng nghĩa lệnh <code>tools/list</code> đổ khoảng 1.000 JSON Schema nặng tới <strong>203.421 tokens</strong> vào prompt ngay cả khi người dùng mới hỏi câu đầu tiên. GAP giữ bề mặt MCP cố định ở đúng <strong>5 tool tĩnh (651 tokens)</strong> bất kể quy mô <code>N=10</code> hay <code>N=1.000</code>:</p>"""
                + svg_progressive_levels("vi")
                + lesson_footer(
                    "mcp-1",
                    [
                        "GAP giữ nguyên đúng 5 công cụ tĩnh trong `tools/list` (`651` tokens) ở mọi quy mô hệ thống.",
                        "Việc tìm hiểu schema chi tiết được chia thành 3 bậc nạp theo nhu cầu (`L0`, `L1`, `L2`).",
                    ],
                    "vi",
                ),
            },
            {
                "id": "mcp-lesson-2",
                "h2": "Bài 2: Làm chủ Quy trình Khám phá Lũy tiến (`L0 -> L1 -> L2 -> gap_query`)",
                "subheadings": [],
                "html": """<p>Trợ lý AI đi qua 3 bước gọn nhẹ — trong đó bước tìm kiếm luôn áp dụng Cedar visibility bitset của người gọi <strong>trước khi xếp hạng điểm BM25/HNSW</strong> (<code>INV-05</code>):</p>"""
                + code_block(
                    '// Tầng L0 Tổng quan (<= 24 tok/nhánh)\n{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"gap_describe","arguments":{"overview":true}}}\n\n// Tầng L1 Tìm kiếm đã lọc quyền\n{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"gap_search","arguments":{"query":"customer orders","limit":5}}}\n\n// Tầng L2 Mở đúng các kiểu đã biên dịch\n{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"gap_describe","arguments":{"types":["shop_CustomersResult","shop_OrdersResult"],"depth":1}}}',
                    "json",
                    "c2-l2-hop",
                    "vi",
                    title="MCP 2026-07-28 — Chuỗi Khám phá L0, L1 và L2",
                )
                + code_challenge(
                    "c2-l2-chal",
                    "Mở Đúng Tên Kiểu Supergraph ở Tầng L2",
                    "<p>Điền chính xác tên kiểu khách hàng đã được OpenAPI biên dịch vào tham số <code>types</code> của lệnh <code>gap_describe</code>:</p>",
                    '{\n  "name": "gap_describe",\n  "arguments": { "types": ["____"], "depth": 1 }\n}',
                    ["shop_CustomersResult", '"shop_CustomersResult"'],
                    ["shop_CustomersResult", "shop_OrdersResult", "Customer"],
                    "Khi nạp OpenAPI, branch_id \"shop\" ghép với kiểu trả về của endpoint cho ra shop_CustomersResult (và shop_OrdersResult). Gọi tên trần Customer sẽ báo lỗi GAP_UNKNOWN_TYPE.",
                    "vi",
                )
                + lesson_footer(
                    "mcp-2",
                    [
                        "Dùng `gap_describe({overview: true})` ở L0, `gap_search` ở L1 và `gap_describe({types: [...]})` ở L2.",
                        "Cedar visibility bitset ẩn toàn bộ tool cấm khỏi bảng xếp hạng lẫn tổng số lượng kết quả (`INV-05`).",
                    ],
                    "vi",
                ),
            },
            {
                "id": "mcp-lesson-3",
                "h2": "Bài 3: Tích hợp Claude, Cursor & Antigravity qua `--stdio` hoặc HTTP",
                "subheadings": [],
                "html": """<p>Các ứng dụng AI trên máy cá nhân kết nối trực tiếp qua JSON-RPC từng dòng ở chế độ <code>--stdio</code> hoặc qua HTTP <code>POST /mcp</code> (kèm header <code>MCP-Protocol-Version: 2026-07-28</code>). Với <code>--stdio</code>, GAP chỉ xuất khung JSON-RPC lên <code>stdout</code> và chuyển toàn bộ log vận hành sang <code>stderr</code>:</p>"""
                + code_block(
                    '{\n  "mcpServers": {\n    "gap-gateway": {\n      "command": "./target/dist/gap",\n      "args": ["serve", "--config", "examples/walking-skeleton/gap.toml", "--stdio"],\n      "env": { "GAP_TOKEN": "gap_dev_skeleton_key_01" }\n    }\n  }\n}',
                    "json",
                    "c2-l3-stdio",
                    "vi",
                    title="Cấu hình Máy chủ MCP cho AI Host (--stdio)",
                )
                + lesson_footer(
                    "mcp-3",
                    [
                        "Chế độ `--stdio` bảo đảm `stdout` sạch 100% khung JSON-RPC trong khi log cấu trúc ghi ra `stderr`.",
                        "Đặt header `MCP-Protocol-Version: 2026-07-28` khi gọi `POST /mcp` qua HTTP/2 hoặc HTTP/3.",
                    ],
                    "vi",
                ),
            },
            {
                "id": "mcp-lesson-4",
                "h2": "Bài 4: Phát Sự kiện Trực quan AG-UI `v1.0` & Nhắc Duyệt Người Dùng",
                "subheadings": [],
                "html": """<p>Giao diện web tương tác kết nối qua luồng Server-Sent Events <code>POST /ag-ui</code>, nhận lần lượt các sự kiện <code>RUN_STARTED</code>, <code>TOOL_CALL_RESULT</code>, <code>STATE_SNAPSHOT</code>, điểm tạm dừng chờ người dùng duyệt, và chuỗi dữ liệu ngoài được bọc kín trong <code>&lt;gap:untrusted&gt;</code>:</p>"""
                + code_block(
                    'curl -sS -N http://127.0.0.1:8080/ag-ui \\\n  -H "Authorization: Bearer gap_dev_skeleton_key_01" \\\n  -H "Content-Type: application/json" \\\n  -d \'{"threadId":"t-1","runId":"r-1","messages":[{"id":"m-1","role":"user","content":"Show VIP customer 1"}],"state":{"generation":1}}\'',
                    "terminal",
                    "c2-l4-agui",
                    "vi",
                    title="Terminal — Theo dõi Luồng SSE AG-UI v1.0 Trực tiếp",
                )
                + practice_check(
                    "c2-q1",
                    "GAP ngăn chặn AI Agent thiếu quyền phát hiện ra sự tồn tại của các tool quản trị bằng cách nào?",
                    [
                        "Áp dụng Cedar visibility bitset của principal TRƯỚC khi chấm điểm BM25/HNSW (`INV-05`), loại bỏ tool bị cấm khỏi danh sách ứng viên và bộ đếm.",
                        "Nhắc LLM bằng system prompt rằng không được gọi các tool admin.",
                        "Cho chạy trước rồi ẩn thông báo lỗi trả về sau.",
                    ],
                    0,
                    "Lọc quyền tất định trước bước xếp hạng (INV-05) bảo đảm hoàn toàn không rò rỉ thông tin tồn tại của tool cấm.",
                    "vi",
                )
                + lesson_footer(
                    "mcp-4",
                    [
                        "`POST /ag-ui` truyền trực tuyến các sự kiện SSE chuẩn với điểm ngắt phê duyệt từ con người.",
                        "Một gateway duy nhất phục vụ đồng thời `/graphql`, `/mcp`, `/ag-ui` và A2A `v1.0`.",
                    ],
                    "vi",
                ),
            },
        ],
        "markdown": """# Khóa 2: MCP, AG-UI & Trợ lý AI

1. Bề mặt 5 tool tĩnh (`651` tokens) thay thế khối JSON Schema phình to `203.421` tokens.
2. Luồng chuẩn: `L0 overview` -> `L1 gap_search` (`INV-05`) -> `L2 gap_describe(types: ["shop_CustomersResult"])` -> `gap_query`.
3. `--stdio` giữ sạch `stdout` cho JSON-RPC; HTTP dùng `MCP-Protocol-Version: 2026-07-28`.
4. `POST /ag-ui` stream SSE và bọc payload lạ trong `<gap:untrusted>`.
""",
    }

    # 6. COURSE 3: REST, SQL & N+1 BATCHING
    pages["course-connectors-batching"] = {
        "lead": (
            "Khóa 3 giúp bạn giải quyết bài toán hiệu năng thực tế trong GAP: bắt bệnh thác nước N+1, cấu hình gom lô khai báo `x-gap-in-param` (`INV-18`), kết nối SQL cùng đồ thị AST mã nguồn `tree-sitter`, và phát hành schema mới an toàn qua quy trình Propose-Not-Activate."
        ),
        "learn_bullets": [
            "Hiểu vì sao join 50 bản ghi bằng LLM phẳng tiêu tốn 4 lượt hội thoại và hàng trăm nghìn tokens.",
            "Cấu hình phần mở rộng OpenAPI `\"x-gap-in-param\"` (`INV-18`) và truy vấn gom nhóm SQL `IN (...)`.",
            "Thành thạo chu trình `gap ingest once` -> `gap proposals list` -> `gap snapshot diff` -> `gap proposals approve`.",
        ],
        "sections": [
            {
                "id": "batch-lesson-1",
                "h2": "Bài 1: Nhận diện Lỗi Thác nước N+1 Trong Hệ thống Agent",
                "subheadings": [],
                "html": _course_banner(
                    "batch",
                    "Khóa 3: REST, SQL & Gom lô N+1",
                    "25 phút",
                    [
                        ("batch-lesson-1", "Thác nước N+1"),
                        ("batch-lesson-2", "x-gap-in-param"),
                        ("batch-lesson-3", "SQL & Code AST"),
                        ("batch-lesson-4", "Duyệt Snapshot"),
                    ],
                )
                + """<p>Khi một LLM sử dụng các tool đơn lẻ để lấy 50 khách hàng rồi vòng lặp gọi đơn hàng và ghi chú cho từng người, mô hình phải đọc lại toàn bộ lịch sử hội thoại qua nhiều lượt trong khi phát ra hàng chục HTTP request rời rạc. Theo số liệu đo thật <code>GAP-Bench</code>, ở <code>N=1.000</code> tool cách làm này tốn tới <strong>814.959,8 tokens</strong> (không cache) so với chỉ <strong>8.547,9 tokens</strong> (-99,0%) của GAP:</p>"""
                + svg_batch_join("vi")
                + lesson_footer(
                    "batch-1",
                    [
                        "Thao tác lặp của LLM biến câu join quan hệ thành hàng loạt lượt gọi chậm và đắt đỏ.",
                        "GAP khử trùng lặp tập khóa ngoại ngay bên trong bộ thực thi `gap-exec`.",
                    ],
                    "vi",
                ),
            },
            {
                "id": "batch-lesson-2",
                "h2": "Bài 2: Khai báo Gom lô REST Bằng `\"x-gap-in-param\"` (`INV-18`)",
                "subheadings": [],
                "html": """<p>Giống cơ chế gom khóa <code>$batch</code> trên các hệ thống REST Connector hiện đại, adapter OpenAPI của GAP nhận diện phần mở rộng <code>"x-gap-in-param"</code> ngay trong đặc tả OpenAPI (trích từ <code>examples/walking-skeleton/shop.openapi.json</code>):</p>"""
                + code_block(
                    '"/customers/{id}": {\n  "get": {\n    "operationId": "customer",\n    "x-gap-in-param": {\n      "path": "/customers",\n      "param": "ids",\n      "key_field": "id",\n      "max_batch_size": 50\n    }\n  }\n}',
                    "json",
                    "c3-l2-batch",
                    "vi",
                    title="examples/walking-skeleton/shop.openapi.json",
                )
                + code_challenge(
                    "c3-x-gap",
                    "Kích hoạt Gom Lô Khóa Cho Endpoint REST",
                    "<p>Điền chính xác tên trường mở rộng OpenAPI được <code>gap-ngc-openapi</code> sử dụng để gom 50 lần gọi rời thành <code>ceil(unique_keys / max_batch_size)</code> lượt gọi (<code>INV-18</code>):</p>",
                    '"operationId": "customer",\n"____": {\n  "path": "/customers",\n  "param": "ids",\n  "key_field": "id",\n  "max_batch_size": 50\n}',
                    ["x-gap-in-param", '"x-gap-in-param"'],
                    ["x-gap-in-param", "x-gap-batch", "x-batch-keys"],
                    "Khai báo \"x-gap-in-param\" hướng dẫn gap-ngc-openapi và gap-exec tự động gộp các khóa {id} đơn lẻ thành /customers?ids=1,2,... giới hạn bởi max_batch_size (INV-18).",
                    "vi",
                )
                + lesson_footer(
                    "batch-2",
                    [
                        "Bất biến `INV-18` đảm bảo mỗi cạnh `@join` chỉ gửi tối đa `ceil(unique_keys / max_batch)` yêu cầu sang nhánh đích.",
                        "Không cần viết mã Rust kết nối thủ công — chỉ cần khai báo JSON trong tệp OpenAPI.",
                    ],
                    "vi",
                ),
            },
            {
                "id": "batch-lesson-3",
                "h2": "Bài 3: Gắn Cơ sở dữ liệu SQL & Đồ thị Mã nguồn `tree-sitter` (`208,7x`)",
                "subheadings": [],
                "html": """<p>Bên cạnh REST, cùng một siêu đồ thị có thể nạp bảng dữ liệu SQL (<code>gap-ngc-sql</code> tự tạo mệnh đề <code>IN (...)</code> tham số hóa chống SQL injection) và mã nguồn phần mềm (<code>gap-ngc-code</code> lập chỉ mục hàm gọi/được gọi qua <code>tree-sitter</code>):</p>"""
                + code_block(
                    '[[branches]]\nid = "codebase"\nprotocol = "code"\ntrust = "T0"\n\n[branches.code]\nroot = "."\nlanguages = ["rust", "python", "typescript"]',
                    "toml",
                    "c3-l3-code",
                    "vi",
                    title="Mẫu Khai báo Nhánh Code AST (TOML)",
                )
                + """<p>Trên bài chuẩn <code>code_graph</code> (24 symbols), việc truy vấn quan hệ mã nguồn qua GAP thay thế 11 lượt tìm đọc file (<code>137.810,4</code> tokens có cache) chỉ còn <strong>1 lượt duy nhất (<code>660,4</code> tokens — hiệu suất gấp <code>208,7x</code>)</strong>.</p>"""
                + lesson_footer(
                    "batch-3",
                    [
                        "`gap-ngc-sql` chuyển các khóa ngoại thành cạnh đồ thị và thực thi qua truy vấn SQL `IN` tham số hóa.",
                        "`gap-ngc-code` phân tích cú pháp AST `tree-sitter` giúp tiết kiệm tới 208,7 lần chi phí duyệt mã.",
                    ],
                    "vi",
                ),
            },
            {
                "id": "batch-lesson-4",
                "h2": "Bài 4: Đưa Schema Mới Vào Chạy An toàn (`Propose-Not-Activate`)",
                "subheadings": [],
                "html": """<p>Không một schema mới hoặc cạnh <code>@join</code> nào tự ý kích hoạt vào đường phục vụ trực tiếp (<code>INV-06</code>). GAP chạy kiểm thử mẫu dữ liệu thực tế cho cạnh nối (không bao giờ tin suy đoán LLM) và chờ người vận hành duyệt trước khi tráo nguyên tử <code>ArcSwap</code>:</p>"""
                + code_block(
                    "./target/dist/gap ingest once --config examples/walking-skeleton/gap.toml --tenant default\n./target/dist/gap proposals list\n./target/dist/gap snapshot diff --before examples/walking-skeleton/gap.toml --after next.toml\n./target/dist/gap proposals approve <proposal-id>",
                    "terminal",
                    "c3-l4-diff",
                    "vi",
                    title="Terminal — Trích xuất, So sánh Diff & Duyệt Proposal",
                )
                + practice_check(
                    "c3-q1",
                    "Điều gì xảy ra nếu `gap ingest` tìm thấy một cạnh `@join` tiềm năng mới, nhưng kỹ sư vận hành chưa chạy lệnh `gap proposals approve`?",
                    [
                        "Cạnh nối giữ ở trạng thái `PROPOSED` và bị loại hoàn toàn khỏi bộ lập kế hoạch Mặt phẳng 1 (`INV-06`).",
                        "GAP tự động kích hoạt sau 30 giây nếu thấy không có lỗi.",
                        "Một mô hình neural tự chọn kích hoạt ngẫu nhiên theo từng request.",
                    ],
                    0,
                    "Chỉ những cạnh nối mang trạng thái ACTIVE đã được người vận hành phê duyệt (sau bước kiểm mẫu) mới được phép tham gia lập kế hoạch thực thi (INV-06).",
                    "vi",
                )
                + lesson_footer(
                    "batch-4",
                    [
                        "Mặt phẳng 1 chỉ lập kế hoạch trên các cạnh `@join` ở trạng thái `ACTIVE` (`INV-06`).",
                        "`gap snapshot diff` giúp kiểm soát toàn bộ thay đổi cấu trúc trước khi nhấn duyệt.",
                    ],
                    "vi",
                ),
            },
        ],
        "markdown": """# Khóa 3: REST, SQL & Gom lô N+1

1. Vòng lặp N+1 tốn tới `814.959` token; `INV-18` chặn trần số request ở mức `ceil(unique_keys / max_batch)`.
2. Khai báo `"x-gap-in-param"` trong OpenAPI để tự động gom lô REST.
3. Gắn chung SQL (`IN (...)`) và Code AST `tree-sitter` (`208,7x` hiệu quả token) vào một siêu đồ thị.
4. Phát hành thay đổi có kiểm soát qua `gap ingest once` -> `gap snapshot diff` -> `gap proposals approve` (`INV-06`).
""",
    }

    # 7. COURSE 4: ENTERPRISE ZERO-TRUST, CEDAR & HTTP/3
    pages["course-enterprise-zero-trust"] = {
        "lead": (
            "Khóa 4 chuẩn bị cho bạn triển khai GAP trong môi trường doanh nghiệp khắc nghiệt: cô lập tool `T2` thiếu tin cậy, thực thi luật Cedar duyệt 2 người cho thao tác `IRREVERSIBLE`, chặn rug-pull & SSRF, và bật HTTP/3 QUIC kèm chuỗi kiểm toán BLAKE3."
        ),
        "learn_bullets": [
            "Áp dụng đúng các tầng tin cậy `T0/T1/T2`, OS sandbox và lớp thoát chuỗi `<gap:untrusted>`.",
            "Viết và kiểm thử luật Cedar trong `policies/default.cedar` (`gap policy check --dir policies`).",
            "Hiểu cơ chế đóng băng `DriftMonitor`, ghim IP `EgressGuard`, tắt `0-RTT` trên HTTP/3 và `gap audit verify`.",
        ],
        "sections": [
            {
                "id": "sec-lesson-1",
                "h2": "Bài 1: Phân tầng Tin cậy (`T0/T1/T2`) & Bọc Dữ liệu `<gap:untrusted>`",
                "subheadings": [],
                "html": _course_banner(
                    "sec",
                    "Khóa 4: Zero-Trust, Cedar & HTTP/3",
                    "25 phút",
                    [
                        ("sec-lesson-1", "T0/T1/T2 & Escaping"),
                        ("sec-lesson-2", "Luật Cedar"),
                        ("sec-lesson-3", "Rug-Pull & SSRF"),
                        ("sec-lesson-4", "HTTP/3 & Audit"),
                    ],
                )
                + """<p>Mỗi nhánh upstream mang một nhãn tin cậy bắt buộc: <code>T0</code> (có chữ ký tổ chức Ed25519 hợp lệ; sai chữ ký lập tức hạ xuống <code>T2</code>), <code>T1</code> (admin phê duyệt và ghim mã băm JCS BLAKE3), hoặc <code>T2</code> (ngoài / chưa tin cậy). Tiến trình <code>T2</code> stdio bắt buộc chạy trong OS sandbox (Bubblewrap/nsjail trên Linux, Seatbelt trên macOS — từ chối chạy nếu thiếu sandbox) và bị cấm nối chéo qua miền khác (<code>INV-07</code>).</p>
<p>Đồng thời, mọi kết quả từ <code>T1/T2</code> đều được bọc trong cặp thẻ <code>&lt;gap:untrusted&gt;...&lt;/gap:untrusted&gt;</code> và thoát hết thẻ đóng giả mạo (<code>SEC-080</code>) nhằm vô hiệu hóa các đòn prompt injection gián tiếp.</p>"""
                + lesson_footer(
                    "sec-1",
                    [
                        "Sai lệch chữ ký Ed25519 tự động hạ cấp an toàn nhánh từ `T0` về `T2`.",
                        "Mọi payload trả ra từ nhánh `T1`/`T2` được bọc an toàn trong `<gap:untrusted>` (`SEC-080`).",
                    ],
                    "vi",
                ),
            },
            {
                "id": "sec-lesson-2",
                "h2": "Bài 2: Luật Phân quyền Cedar Duyệt 2 Người (`INV-04`, `INV-11`)",
                "subheadings": [],
                "html": """<p>Trong GAP, mọi quyết định cho phép hay từ chối hoàn toàn tách khỏi suy đoán của LLM (<code>INV-04</code>). Bộ đánh giá Cedar thuần hàm chạy trên <code>(principal, action, resource, context)</code>, trong đó luật <code>forbid</code> luôn thắng <code>permit</code>. Xem quy tắc gốc tại <code>policies/default.cedar</code>:</p>"""
                + code_block(
                    '@id("forbid-mutate-irreversible-or-unknown-unapproved")\nforbid (\n    principal,\n    action == Action::"Mutate",\n    resource\n) when {\n    (resource.sideEffect == "IRREVERSIBLE" || resource.sideEffect == "UNKNOWN")\n        && !(context.approved && context.approverIsDifferent)\n};\n\n# Kiểm tra cú pháp & kiểu Cedar offline:\n./target/dist/gap policy check --dir policies',
                    "cedar",
                    "c4-l2-cedar",
                    "vi",
                    title="policies/default.cedar & Lệnh Kiểm định Policy",
                )
                + code_challenge(
                    "c4-cedar-chal",
                    "Bắt Buộc Người Duyệt Độc lập Thứ Hai Trong Cedar",
                    "<p>Điền điều kiện bối cảnh cuối cùng của luật <code>policies/default.cedar</code> để người yêu cầu không thể tự phê duyệt thao tác <code>IRREVERSIBLE</code> của chính mình:</p>",
                    "when {\n  (resource.sideEffect == \"IRREVERSIBLE\" || resource.sideEffect == \"UNKNOWN\")\n    && !(context.approved && context.____)\n}",
                    ["approverIsDifferent"],
                    ["approverIsDifferent", "isAdmin", "mfaVerified"],
                    "Điều kiện context.approverIsDifferent đảm bảo người ký duyệt phải mang danh tính khác với principal gửi yêu cầu.",
                    "vi",
                )
                + lesson_footer(
                    "sec-2",
                    [
                        "Trong Cedar, bất kỳ điều khoản `forbid` khớp nào cũng lập tức thắng toàn bộ `permit`.",
                        "Chạy `gap policy check --dir policies` trong CI trước khi nạp nóng bằng `gap policy reload`.",
                    ],
                    "vi",
                ),
            },
            {
                "id": "sec-lesson-3",
                "h2": "Bài 3: Chặn MCP Rug-Pull (`DriftMonitor`) & SSRF (`EgressGuard`)",
                "subheadings": [],
                "html": """<ul>
  <li><strong>Chống Rug-Pull Mô tả Tool (<code>SEC-082</code>, <code>INV-08</code>):</strong> Nếu một dịch vụ ngoài lén sửa mô tả công cụ để nhét chỉ thị ẩn sau khi quản trị viên đã duyệt, <code>DriftMonitor</code> phát hiện sai lệch mã băm JCS BLAKE3 <code>desc_hash</code>, đóng băng nhánh ngay về <code>BranchStatus::Drift</code> và chỉ cung cấp cho Agent đúng mô tả cũ đã ghim.</li>
  <li><strong>Phòng vệ SSRF Không Kẽ hở (<code>SEC-075</code>):</strong> Trước khi mở kết nối ra ngoài, <code>EgressGuard</code> phân giải DNS đúng một lần duy nhất, loại bỏ toàn bộ địa chỉ nội bộ/loopback/RFC1918/dạng bát phân/<code>169.254.169.254</code>, ghim kết nối vào chính IP sạch đó chống DNS rebinding và khóa chuyển hướng HTTP redirect.</li>
</ul>"""
                + lesson_footer(
                    "sec-3",
                    [
                        "`DriftMonitor` ngăn mọi thay đổi lén lút trên mô tả tool lọt vào ngữ cảnh Agent.",
                        "`EgressGuard` khóa chặt mọi ngả tấn công DNS rebinding, metadata đám mây và HTTP redirect.",
                    ],
                    "vi",
                ),
            },
            {
                "id": "sec-lesson-4",
                "h2": "Bài 4: HTTP/3 QUIC, Chuỗi Băm BLAKE3 & Phục hồi Sự cố (`28,86 ms`)",
                "subheadings": [],
                "html": """<p>Khi lên production, bật song song HTTP/2 và HTTP/3 trên UDP QUIC (đo thực tế đạt <strong>5.013 req/s</strong> ở <code>2,36 ms</code> p95, <strong>0 lỗi</strong>, và phục hồi <code>/readyz</code> chỉ trong <strong>28,86 ms</strong> sau khi bị <code>SIGKILL</code>), với <code>max_early_data_size = 0</code> tắt hoàn toàn rủi ro phát lại <code>0-RTT</code>:</p>"""
                + svg_facade_flow("vi")
                + code_block(
                    "./target/dist/gap audit verify --nightly\n./target/dist/gap audit export --format jsonld > prov-o-audit.json",
                    "terminal",
                    "c4-l4-audit",
                    "vi",
                    title="Terminal — Xác minh Chuỗi Băm BLAKE3 & Xuất JSON-LD PROV-O",
                )
                + practice_check(
                    "c4-q1",
                    "Vì sao GAP bắt buộc đặt `max_early_data_size = 0` (tắt TLS 1.3 `0-RTT` trên QUIC HTTP/3)?",
                    [
                        "Vì gói tin `0-RTT` trong TLS 1.3 có thể bị kẻ tấn công phát lại (replay) trên mạng và làm chạy lặp thao tác `gap_mutate`.",
                        "Vì giao thức QUIC không hỗ trợ ALPN `h3` nếu bật `0-RTT`.",
                        "Để ép mọi trình duyệt quay ngược về dùng HTTP/1.1.",
                    ],
                    0,
                    "Tắt 0-RTT mang lại miễn nhiễm replay attack tuyệt đối cho các mutation mà vẫn đạt thông lượng 5.013 req/s trên HTTP/3.",
                    "vi",
                )
                + lesson_footer(
                    "sec-4",
                    [
                        "Chế độ `profile = \"prod\"` từ chối chạy nếu thiếu cấu hình chứng chỉ `tls_cert_secret` và `tls_key_secret`.",
                        "`gap audit verify --nightly` kiểm tra tính toàn vẹn chuỗi băm BLAKE3 của toàn bộ nhật ký kiểm toán.",
                    ],
                    "vi",
                ),
            },
        ],
        "markdown": """# Khóa 4: Zero-Trust, Cedar & HTTP/3

1. Xác minh chữ ký `T0` thất bại sẽ hạ nhánh về `T2`; `T2` chạy trong OS sandbox và bọc `<gap:untrusted>`.
2. `policies/default.cedar` chặn `IRREVERSIBLE`/`UNKNOWN` nếu thiếu `context.approved && context.approverIsDifferent`.
3. `DriftMonitor` đóng băng rug-pull (`INV-08`); `EgressGuard` chặn DNS rebinding & metadata SSRF (`169.254.169.254`).
4. Dual-stack HTTP/2 + HTTP/3 QUIC tắt `0-RTT` (`max_early_data_size = 0`) và ghi nhận kiểm toán vào chuỗi BLAKE3 (`gap audit verify`).
""",
    }

    return pages
