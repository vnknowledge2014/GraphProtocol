"""English Learn Basics (`graphql.com/learn`) & GAP Academy (`apollographql.com/tutorials`) pages."""
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
    return lambda target: link_to_page(slug, target, "en")


def _course_banner(course_id: str, title: str, duration: str, lessons: list[tuple[str, str]]) -> str:
    pills = "".join(
        f'<a class="lesson-step-pill" href="#{esc(sec_id)}" data-lesson-pill="{esc(course_id)}-{idx + 1}">'
        f'<span class="step-num">{idx + 1}</span><span>{esc(label)}</span></a>'
        for idx, (sec_id, label) in enumerate(lessons)
    )
    return f"""<div class="course-banner" data-course-id="{esc(course_id)}">
  <div class="course-banner-top">
    <span><strong>{esc(title)}</strong> · {esc(duration)} · {len(lessons)} lessons</span>
    <span class="pill" data-course-progress="{esc(course_id)}">0 / {len(lessons)} completed</span>
  </div>
  <nav class="lesson-stepper" aria-label="Course lessons">{pills}</nav>
</div>"""


def build_academy_en() -> dict[str, dict]:
    pages: dict[str, dict] = {}

    # 1. LEARN BASICS — PART 1: GRAPHQL & GAP FOUNDATIONS
    u = _ctx("learn-graphql-and-gap")
    pages["learn-graphql-and-gap"] = {
        "lead": (
            "New to GraphQL or agent gateways? This zero-jargon guide builds your intuition from scratch: why AI agents benefit from one unified menu, how Types work like labeled boxes, and how a query mirrors JSON 1:1."
        ),
        "learn_bullets": [
            "Visualize a GAP Supergraph as a single coordinated order counter in front of REST, SQL, gRPC, MCP, and Code backends.",
            "Read schemas effortlessly: Object types (`shop_CustomersResult`), Scalars (`Int`, `String`, `JSON`), Enums, Lists, and Non-Null (`!`).",
            "Use the Interactive Query Builder below to toggle fields and watch valid GAP-QL match its JSON output 1:1.",
        ],
        "sections": [
            {
                "id": "what-is-a-supergraph",
                "h2": "Mental Model: One Order Counter vs. Twenty Separate Vendors",
                "subheadings": [],
                "html": """<p>Imagine an AI assistant is a dinner guest arriving at a busy food court with twenty specialized kitchens: one serves customer profiles (REST API), another tracks shipping orders (SQL database), and a third keeps support notes (MCP server).</p>
<ul>
  <li><strong>Without GAP (Flat Direct-MCP):</strong> Before taking a single order, the host hands the agent every binder from all 20 kitchens at once—hundreds of thousands of tokens of repetitive JSON schemas. Worse, combining a customer with their notes forces the agent to walk back and forth turn-by-turn.</li>
  <li><strong>With GAP (A Typed Supergraph):</strong> GAP places <strong>one unified, menu-indexed order counter</strong> in front of every backend. Instead of memorizing every recipe up front (<code>203,421</code> tokens at 1,000 tools), the agent holds <strong>5 tiny ordering tools (651 tokens)</strong>, looks up only the dish it needs, and submits one structured request.</li>
</ul>"""
                + callout(
                    "note",
                    "Supergraph & Branch in Plain English",
                    "<p>Think of the <strong>Supergraph</strong> as the combined menu compiled by GAP, and each backend kitchen (<code>shop</code>, <code>notes</code>, <code>billing_sql</code>) as one <strong>Branch</strong> inside that menu.</p>",
                ),
            },
            {
                "id": "types-scalars-nullability",
                "h2": "Types Are Labeled Shipping Boxes (`!` Means Guaranteed)",
                "subheadings": [],
                "html": """<p>Every GraphQL and GAP schema is just a contract describing which labeled boxes exist and what fits inside each box:</p>"""
                + data_table(
                    "Schema building blocks illustrated with Walking Skeleton examples",
                    ["Building Block", "Schema Example", "Plain Meaning"],
                    [
                        ["<strong>Object Type</strong>", "<code>type shop_CustomersResult &#123; id: Int! name: String! tier: ... &#125;</code>", "A structured record representing one domain entity (like a customer row from OpenAPI)."],
                        ["<strong>Scalar</strong>", "<code>Int</code>, <code>String</code>, <code>Boolean</code>, <code>JSON</code>", "A leaf value where selection stops—there are no deeper nested <code>&#123; ... &#125;</code> subfields inside a scalar."],
                        ["<strong>Non-Null (<code>!</code>)</strong>", "<code>id: Int!</code> vs. <code>name: String</code>", "The exclamation mark guarantees GAP will never return <code>null</code> for that field on success."],
                        ["<strong>List (<code>[...]</code>)</strong>", "<code>orders(customer_id: Int): [shop_OrdersResult!]</code>", "Returns zero or more items of that type in an array."],
                        ["<strong>Enum</strong>", "<code>shop_CustomersResultTier</code> (<code>vip</code>, <code>standard</code>)", "A fixed allowlist of string values validated before hitting backends."],
                    ],
                )
                + going_deeper(
                    "Going deeper: Why does `customer_notes(customer_id: 1)` have NO curly braces after it?",
                    "<p>In <code>examples/walking-skeleton</code>, the OpenAPI REST endpoints (<code>shop_CustomersResult</code>, <code>shop_OrdersResult</code>) compile into strongly typed Object types whose subfields you select inside <code>&#123; id name tier &#125;</code>. By contrast, external MCP tool <code>customer_notes(customer_id: Int!): JSON</code> returns the custom <code>JSON</code> leaf scalar—so writing <code>customer_notes(customer_id: 1) &#123; notes &#125;</code> triggers a GraphQL validation error! Treat scalars as leaves with zero brace sub-selection.</p>",
                ),
            },
            {
                "id": "query-mirrors-json",
                "h2": "How a Query Mirrors Its JSON 1:1 (Interactive Builder)",
                "subheadings": [],
                "html": """<p>Unlike REST URLs where the server guesses which 40 fields you want, a GAP-QL query acts like a fillable JSON template: <strong>every field you ask for comes back in the exact same nesting shape, and unrequested fields stay off the wire</strong>.</p>"""
                + svg_query_mirror("en")
                + f"""<div class="query-builder" id="query-builder-widget">
  <div class="qb-header">
    <h3>Click Fields to Build a Live Walking Skeleton Query</h3>
    <span class="pill" id="qb-stats-pill">1 GAP request · vs. 3 Direct calls</span>
  </div>
  <div class="qb-controls">
    <span class="qb-group-title">Select fields:</span>
    <div class="qb-check-row">
      <input type="checkbox" id="qb-id" checked disabled />
      <label for="qb-id"><code>customer.id</code> (always included)</label>
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
      <label for="qb-orders"><code>orders(customer_id: 1)</code> (shop OpenAPI)</label>
    </div>
    <div class="qb-check-row">
      <input type="checkbox" id="qb-notes" checked />
      <label for="qb-notes"><code>customer_notes(customer_id: 1)</code> (MCP JSON scalar)</label>
    </div>
  </div>
  <div class="qb-panes">
    <figure class="code-figure">
      <div class="code-toolbar">
        <div class="code-meta">
          <span class="code-lang">graphql</span>
          <span class="code-title">Generated GAP-QL Request</span>
        </div>
        <span class="copy">
          <button type="button" class="copy-button" command="--copy" commandfor="qb-graphql-code" aria-label="Copy generated GAP-QL query">
            <span class="icon" aria-hidden="true"></span>
          </button>
          <span class="tip" role="status"></span>
        </span>
      </div>
      <pre role="region" aria-label="Generated GAP-QL query" tabindex="0"><code id="qb-graphql-code" class="language-graphql">query &#123;
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
          <span class="code-title">Exact Mirrored Output</span>
        </div>
        <span class="copy">
          <button type="button" class="copy-button" command="--copy" commandfor="qb-json-code" aria-label="Copy mirrored JSON response">
            <span class="icon" aria-hidden="true"></span>
          </button>
          <span class="tip" role="status"></span>
        </span>
      </div>
      <pre role="region" aria-label="Mirrored JSON output" tabindex="0"><code id="qb-json-code" class="language-json">&#123;
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
  <p class="section-lead" style="margin-top:0.65rem;">Continue to <a href="{esc(u('learn-queries-mutations-joins'))}">Arguments, Mutations &amp; Joins</a> or start coding right away in <a href="{esc(u('academy-hub'))}">GAP Academy</a>.</p>
</div>""",
            },
        ],
        "markdown": """# GraphQL & GAP Basics

- **Supergraph & Branches**: GAP acts as a unified typed menu in front of REST, SQL, GraphQL, gRPC, CLI, Code AST, and MCP branches.
- **Types & Nullability**: Object fields open `{ ... }`; scalars (`Int`, `String`, `JSON`) stop selection immediately; `!` guarantees non-null values.
- **1:1 Shape Mirroring**: Queries match returned JSON shapes exactly, fetching `shop` + `notes` in one network round-trip.
""",
    }

    # 2. LEARN BASICS — PART 2: ARGUMENTS, MUTATIONS & JOINS
    u = _ctx("learn-queries-mutations-joins")
    pages["learn-queries-mutations-joins"] = {
        "lead": (
            "Master filtering with variables, writing safely via `@sideEffect` mutations, resolving cross-branch joins in batches, and translating REST or Direct-MCP habits into clean GAP workflows."
        ),
        "learn_bullets": [
            "Pass typed GraphQL `variables` instead of string formatting so `PlanCache` reuses compiled query plans safely.",
            "Distinguish `query` (read-only) from `@sideEffect` `mutation` calls gated by `dry_run` and Cedar two-person rules.",
            "Map every REST / Direct-MCP concept you already know to its exact counterpart in GAP.",
        ],
        "sections": [
            {
                "id": "arguments-and-variables",
                "h2": "Arguments & Parameterized Variables (`PlanCache` Reuse)",
                "subheadings": [],
                "html": """<p>Just like query parameters in a REST URL, GraphQL fields accept typed arguments such as <code>customer(id: 1)</code> or <code>orders(customer_id: 1)</code>. Never concatenate user or LLM inputs directly into query text—declare strongly typed variables (<code>$cid: Int!</code>):</p>"""
                + code_block(
                    'query GetCustomerDossier($cid: Int!) {\n  customer(id: $cid) { id name tier }\n  orders(customer_id: $cid) { id status total_cents }\n  customer_notes(customer_id: $cid)\n}\n\n# Variables JSON:\n# { "cid": 1 }',
                    "graphql",
                    "lrn-vars",
                    "en",
                    title="Parameterized Query + Variables",
                )
                + """<ul>
  <li><strong>Zero Injection:</strong> Typed variables prevent syntax smuggling inside query strings.</li>
  <li><strong>Hot Cache Hits:</strong> Because the query text stays identical across customers, GAP reuses the compiled plan in <code>PlanCache</code> keyed by <code>(TenantId, snapshot_generation, query_hash)</code> (<code>INV-09</code>).</li>
</ul>""",
            },
            {
                "id": "mutations-and-side-effects",
                "h2": "Mutations & `@sideEffect` Safety Rails",
                "subheadings": [],
                "html": """<p>While <code>query</code> operations read data cleanly across snapshots, state-changing actions must use <code>mutation</code> (or MCP tool <code>gap_mutate</code>) annotated with <code>@sideEffect</code>:</p>"""
                + data_table(
                    "GAP @sideEffect levels from spec/02-gap-ql.md §5 and Cedar enforcement",
                    ["SideEffectLevel", "Meaning", "Default Cedar Policy Behavior"],
                    [
                        ["<code>NONE</code>", "Read-only lookup with no state change.", "Permitted for authorized <code>Read</code> principals."],
                        ["<code>REVERSIBLE</code>", "Draft update or compensating write (can undo).", "Allowed with <code>Mutate</code> permission; logged to BLAKE3 ledger."],
                        ["<code>IRREVERSIBLE</code> / <code>UNKNOWN</code>", "Payment transfer, ledger delete, or unverified tool.", "Blocked unless <code>context.approved &amp;&amp; context.approverIsDifferent</code> holds."],
                    ],
                )
                + code_block(
                    '{\n  "jsonrpc": "2.0",\n  "id": 10,\n  "method": "tools/call",\n  "params": {\n    "name": "gap_mutate",\n    "arguments": {\n      "query": "mutation { issue_refund(order_id: 101) { ok } }",\n      "dry_run": true\n    }\n  }\n}',
                    "json",
                    "lrn-dryrun",
                    "en",
                    title="MCP 2026-07-28 — Safe dry_run Preview via gap_mutate",
                ),
            },
            {
                "id": "joins-and-mental-model-map",
                "h2": "GAP for REST & Direct-MCP Developers (Translation Table)",
                "subheadings": [],
                "html": """<p>You do not throw away your existing REST APIs or MCP servers to adopt GAP—you keep them running unchanged and mount them as declarative NGC branches.</p>"""
                + data_table(
                    "Translating REST and Direct-MCP engineering habits to GAP",
                    ["What You Know (REST / Direct-MCP)", "Exact GAP Counterpart", "Key Engineering Upgrade"],
                    [
                        ["Separate HTTP base URLs / multiple MCP configs", "Single gateway URL (<code>/graphql</code>, <code>/mcp</code>, <code>/ag-ui</code>)", "One auth layer, TLS 1.3 QUIC transport, and snapshot boundary."],
                        ["Fetching <code>tools/list</code> with 500 JSON schemas", "Static 5 tools (<code>651 tokens</code>) + <code>gap_describe</code> / <code>gap_search</code>", "Cuts schema tax by 99% while hiding unauthorized ops (<code>INV-05</code>)."],
                        ["Writing client loops <code>for id in ids: fetch(...)</code>", "<code>@join</code> + OpenAPI <code>x-gap-in-param</code> DataLoader batching", "Deduplicates keys into <code>ceil(K/max_batch)</code> calls (<code>INV-18</code>)."],
                        ["Hand-rolled regex ACLs before tool calls", "Deterministic Cedar rules (<code>policies/default.cedar</code>)", "Default-deny pure checks with two-person approval for risky writes."],
                        ["Trusting upstream tool docstrings blindly", "JCS BLAKE3 <code>desc_hash</code> pinning + <code>&lt;gap:untrusted&gt;</code>", "Freezes rug-pull mutations automatically into <code>BranchStatus::Drift</code>."],
                    ],
                )
                + going_deeper(
                    "Going deeper: Why did GAP choose standard GraphQL instead of a brand-new query language?",
                    f"""<p>Per <code>GQL-001</code> and <code>spec/02-gap-ql.md</code>, inventing custom keywords like <code>traverse</code> would break every IDE highlighter, schema linter, and validation library. Using 100% standard GraphQL lets GAP validate queries with Apollo's Rust <code>apollo-compiler</code>, pass 136/136 differential conformance checks against <code>graphql-js 16.13.2</code>, and express federation cleanly with <code>@join</code> and <code>@sideEffect</code> directives. Ready to practice? Head to <a href="{esc(u('academy-hub'))}">GAP Academy</a>.</p>""",
                ),
            },
        ],
        "markdown": """# Arguments, Mutations & Joins

- Always send GraphQL `variables` (`$cid: Int!`) so `PlanCache` reuses compiled plans cleanly per `(TenantId, generation, query_hash)`.
- Mutations require `@sideEffect` classification (`NONE`, `REVERSIBLE`, `IRREVERSIBLE`) and `dry_run` support; `IRREVERSIBLE` requires two-person Cedar approval.
- Existing REST OpenAPI & MCP servers become declarative branches—no client rewrites or custom GraphQL syntax needed.
""",
    }

    # 3. GAP ACADEMY HUB (ODYSSEY COURSE CATALOG)
    u = _ctx("academy-hub")
    pages["academy-hub"] = {
        "lead": (
            "Welcome to GAP Academy — structured, multi-lesson hands-on courses modeled on Apollo Odyssey. Combine protocol theory with real CLI commands, complete fill-in-the-blank challenges, pass knowledge checks, and track mastery across all 16 lessons."
        ),
        "learn_bullets": [
            "Work sequentially through 4 project-driven courses (`16` lessons total) from first supergraph to HTTP/3 production.",
            "Test yourself interactively inside every lesson with Fill-in-the-Blank code challenges and Multiple-Choice checks.",
            "Save lesson completions automatically in `localStorage` across both English and Vietnamese editions.",
        ],
        "sections": [
            {
                "id": "academy-progress",
                "h2": "Your Academy Progress",
                "subheadings": [],
                "html": """<div class="academy-progress-banner" id="academy-hub-progress">
  <div class="academy-prog-top">
    <div>
      <strong>Overall Academy Mastery:</strong> <span id="academy-total-count">0 / 16</span> lessons completed
    </div>
    <button type="button" id="academy-reset-btn" class="btn btn-sm">Reset Progress</button>
  </div>
  <div class="progress-track" aria-hidden="true">
    <div id="academy-total-fill" class="progress-fill"></div>
  </div>
  <p class="section-lead" style="margin:0.45rem 0 0 0;">New to schemas and JSON mirroring? Complete <a href="../learn/graphql-and-gap-basics.html">GraphQL &amp; GAP Basics</a> first, then start Course 1 below.</p>
</div>""",
            },
            {
                "id": "course-catalog",
                "h2": "Choose Your Hands-On Course (4 Tracks · 16 Lessons)",
                "subheadings": [],
                "html": f"""<div class="academy-course-cards">
  <article class="ac-card ac-card--1">
    <div class="ac-card-header">
      <h3>Course 1: Lift-Off</h3>
      <div class="ac-lessons">Beginner · 20 min · 4 lessons</div>
    </div>
    <div class="ac-card-body">
      <p>Scaffold a workspace with <code>gap init</code>, connect OpenAPI <code>shop</code> and MCP <code>notes</code> fixtures, verify health with <code>gap doctor</code>, and execute a cross-branch GraphQL query.</p>
      <div class="ac-card-footer">
        <a class="ac-card-btn" href="{esc(u('course-liftoff'))}">Start Course 1 &rarr;</a>
        <span class="ac-difficulty">Beginner</span>
      </div>
      <span class="pill" data-course-progress="liftoff" style="display:block;margin-top:0.75rem;">0 / 4 lessons</span>
    </div>
  </article>

  <article class="ac-card ac-card--2">
    <div class="ac-card-header">
      <h3>Course 2: MCP, AG-UI &amp; AI Assistants</h3>
      <div class="ac-lessons">AI Agents · 20 min · 4 lessons</div>
    </div>
    <div class="ac-card-body">
      <p>Replace <code>203,421</code>-token tool dumps with <code>651</code>-token progressive discovery (<code>L0→L1→L2</code>), wire Claude/Cursor <code>--stdio</code>, and stream AG-UI <code>v1.0</code> SSE interrupts.</p>
      <div class="ac-card-footer">
        <a class="ac-card-btn" href="{esc(u('course-mcp-agents'))}">Start Course 2 &rarr;</a>
        <span class="ac-difficulty">Intermediate</span>
      </div>
      <span class="pill" data-course-progress="mcp" style="display:block;margin-top:0.75rem;">0 / 4 lessons</span>
    </div>
  </article>

  <article class="ac-card ac-card--3">
    <div class="ac-card-header">
      <h3>Course 3: REST, SQL &amp; N+1 Batching</h3>
      <div class="ac-lessons">Federation · 25 min · 4 lessons</div>
    </div>
    <div class="ac-card-body">
      <p>Eliminate LLM N+1 loops using declarative OpenAPI <code>x-gap-in-param</code> (<code>INV-18</code>), SQL &amp; <code>tree-sitter</code> Code AST branches, and Propose-Not-Activate rollouts.</p>
      <div class="ac-card-footer">
        <a class="ac-card-btn" href="{esc(u('course-connectors-batching'))}">Start Course 3 &rarr;</a>
        <span class="ac-difficulty">Intermediate</span>
      </div>
      <span class="pill" data-course-progress="batch" style="display:block;margin-top:0.75rem;">0 / 4 lessons</span>
    </div>
  </article>

  <article class="ac-card ac-card--4">
    <div class="ac-card-header">
      <h3>Course 4: Zero-Trust, Cedar &amp; HTTP/3</h3>
      <div class="ac-lessons">Production · 25 min · 4 lessons</div>
    </div>
    <div class="ac-card-body">
      <p>Harden your gateway with <code>T0/T1/T2</code> isolation, two-person Cedar rules, anti-rug-pull <code>DriftMonitor</code>, SSRF <code>EgressGuard</code>, HTTP/3 QUIC, and BLAKE3 audits.</p>
      <div class="ac-card-footer">
        <a class="ac-card-btn" href="{esc(u('course-enterprise-zero-trust'))}">Start Course 4 &rarr;</a>
        <span class="ac-difficulty">Advanced</span>
      </div>
      <span class="pill" data-course-progress="sec" style="display:block;margin-top:0.75rem;">0 / 4 lessons</span>
    </div>
  </article>
</div>""",
            },
        ],
        "markdown": """# GAP Academy — Hands-On Courses

4 multi-lesson interactive courses (16 lessons total):
1. [Course 1: Lift-Off — Build Your First Supergraph](liftoff-first-supergraph.html)
2. [Course 2: MCP, AG-UI & AI Assistants](mcp-and-ai-assistants.html)
3. [Course 3: REST, SQL & N+1 Batching](connectors-and-batching.html)
4. [Course 4: Zero-Trust, Cedar & HTTP/3](enterprise-zero-trust.html)
""",
    }

    # 4. COURSE 1: LIFT-OFF — FIRST SUPERGRAPH
    u = _ctx("course-liftoff")
    pages["course-liftoff"] = {
        "lead": (
            "Course 1 takes you from an empty terminal to a live two-branch supergraph combining an OpenAPI shop service and an MCP notes server behind `/graphql` and `/mcp`."
        ),
        "learn_bullets": [
            "Scaffold starter files with `./target/dist/gap init` and understand BLAKE3 key digests.",
            "Run fail-closed validation (`gap config check` and `gap doctor`) before starting listeners.",
            "Query OpenAPI Objects and MCP `JSON` scalars together without hitting `GAP_UNKNOWN_FIELD`.",
        ],
        "sections": [
            {
                "id": "liftoff-lesson-1",
                "h2": "Lesson 1: Scaffolding a Workspace (`gap init`)",
                "subheadings": [],
                "html": _course_banner(
                    "liftoff",
                    "Course 1: Lift-Off",
                    "20 min",
                    [
                        ("liftoff-lesson-1", "Scaffold"),
                        ("liftoff-lesson-2", "Config & Doctor"),
                        ("liftoff-lesson-3", "Boot & Probe"),
                        ("liftoff-lesson-4", "Joined Query"),
                    ],
                )
                + """<p>In real production setups, you never hand-write TOML boilerplate from scratch. Running <code>gap init</code> creates a ready-to-edit 5-file starter layout containing <code>gap.toml</code>, a sample OpenAPI spec, fixture scripts, and a development bearer token hashed with <strong>BLAKE3-256</strong>:</p>"""
                + code_block(
                    "./target/dist/gap init my-shop-gateway\nls -la my-shop-gateway",
                    "terminal",
                    "c1-l1-init",
                    "en",
                    title="Terminal — Scaffold Starter Gateway",
                )
                + callout(
                    "note",
                    "Zero Raw Keys in Config Files",
                    "<p><code>gap.toml</code> stores only <code>key_hash = \"8a7d...\"</code> (for key <code>gap_dev_skeleton_key_01</code> in Walking Skeleton). GAP verifies incoming <code>Authorization: Bearer ...</code> tokens using constant-time BLAKE3 comparison and redacts secrets cleanly (<code>INV-15</code>).</p>",
                )
                + lesson_footer(
                    "liftoff-1",
                    [
                        "`gap init <dir>` generates a working starter supergraph cleanly.",
                        "`gap.toml` stores BLAKE3 hashes (`key_hash`), never plaintext production tokens.",
                    ],
                    "en",
                ),
            },
            {
                "id": "liftoff-lesson-2",
                "h2": "Lesson 2: Wiring OpenAPI & MCP Branches (`gap config check` & `doctor`)",
                "subheadings": [],
                "html": """<p>Open <code>examples/walking-skeleton/gap.toml</code>. Notice two <code>[[branches]]</code> entries: <code>id = "shop"</code> (OpenAPI 3.0 on port <code>9101</code>) and <code>id = "notes"</code> (MCP HTTP on port <code>9102/mcp</code>). Validate both files and OS readiness before running traffic:</p>"""
                + code_block(
                    "./target/dist/gap config check examples/walking-skeleton/gap.toml\n./target/dist/gap doctor --config examples/walking-skeleton/gap.toml",
                    "terminal",
                    "c1-l2-doc",
                    "en",
                    title="Terminal — Validate Config & Run Diagnostics",
                )
                + code_block(
                    "ok: examples/walking-skeleton/gap.toml (profile=dev, branches=2, api_keys=1)\n[ok] config.parse: parsed examples/walking-skeleton/gap.toml (profile=dev)\n[ok] config.files: all referenced branch files exist and are readable\n[ok] server.listen: valid socket address 127.0.0.1:8080\n[ok] auth.api_keys: 1 API key(s) configured",
                    "text",
                    "c1-l2-out",
                    "en",
                    title="Expected Diagnostic Output",
                )
                + lesson_footer(
                    "liftoff-2",
                    [
                        "`gap config check` fails closed on invalid schemas, ports, or missing files.",
                        "`gap doctor` audits sockets, branch file paths, API keys, and sandbox readiness.",
                    ],
                    "en",
                ),
            },
            {
                "id": "liftoff-lesson-3",
                "h2": "Lesson 3: Booting Fixtures & Probing `/readyz`",
                "subheadings": [],
                "html": """<p>Start the two lightweight Python upstream fixtures first, then launch <code>gap serve</code> and inspect <code>/healthz</code> and <code>/readyz</code>:</p>"""
                + code_block(
                    "python3 examples/walking-skeleton/fixtures/rest_fixture.py --port 9101 &\npython3 examples/walking-skeleton/fixtures/mcp_fixture.py --port 9102 &\n./target/dist/gap serve --config examples/walking-skeleton/gap.toml &\ncurl -sS http://127.0.0.1:8080/readyz",
                    "terminal",
                    "c1-l3-boot",
                    "en",
                    title="Terminal — Launch Fixtures, Gateway & Probe /readyz",
                )
                + """<p><code>/readyz</code> returns <code>&#123;"status":"ready","generation":1&#125;</code> only after snapshot generation 1 compiles and binds into the lock-free <code>ArcSwap</code> pointer.</p>"""
                + lesson_footer(
                    "liftoff-3",
                    [
                        "`/healthz` tracks process liveness; `/readyz` turns ready only once Snapshot generation >= 1 compiles.",
                        "Alternatively, click Start Live Lab inside the Interactive Lab page to automate all 3 processes.",
                    ],
                    "en",
                ),
            },
            {
                "id": "liftoff-lesson-4",
                "h2": "Lesson 4: First Cross-Branch Query & Hands-On Check",
                "subheadings": [],
                "html": """<p>Now send one authenticated <code>POST /graphql</code> request querying customer <code>1</code> from the REST branch, their orders from REST, and support notes from the MCP branch simultaneously:</p>"""
                + code_block(
                    'curl -sS http://127.0.0.1:8080/graphql \\\n  -H "Content-Type: application/json" \\\n  -H "Authorization: Bearer gap_dev_skeleton_key_01" \\\n  -d \'{"query": "{ customer(id: 1) { id name tier } orders(customer_id: 1) { id status total_cents } customer_notes(customer_id: 1) }"}\'',
                    "terminal",
                    "c1-l4-curl",
                    "en",
                    title="Terminal — Query REST shop + MCP notes in One Request",
                )
                + code_challenge(
                    "c1-leaf",
                    "Fix the Query Parameter Name",
                    "<p>Complete the cross-branch query so both <code>shop</code> customer #1 and MCP <code>notes</code> for customer #1 resolve together (remember: <code>customer_notes</code> is a <code>JSON</code> scalar!). What argument name goes inside <code>customer_notes(____: 1)</code>?</p>",
                    "query {\n  customer(id: 1) { id name tier }\n  customer_notes(____: 1)\n}",
                    ["customer_id"],
                    ["customer_id", "id", "cust_id"],
                    "In examples/walking-skeleton/fixtures/mcp_fixture.py, the MCP tool accepts customer_id and returns a leaf JSON scalar (no { ... } sub-selection).",
                    "en",
                )
                + practice_check(
                    "c1-q1",
                    "Why does GAP reject `customer_notes(customer_id: 1) { notes }` during validation?",
                    [
                        "Because `customer_notes` returns a `JSON` leaf scalar, and scalars in GraphQL cannot take a `{ ... }` sub-selection.",
                        "Because GAP disables querying MCP branches over `POST /graphql`.",
                        "Because only mutations can query MCP branches.",
                    ],
                    0,
                    "Scalars terminate selection trees in GraphQL. Only Object/Interface/Union types open { ... } braces.",
                    "en",
                )
                + lesson_footer(
                    "liftoff-4",
                    [
                        "A single `POST /graphql` query executes across REST and MCP branches concurrently.",
                        "Continue to Course 2: MCP, AG-UI & AI Assistants next.",
                    ],
                    "en",
                ),
            },
        ],
        "markdown": """# Course 1: Lift-Off — Build Your First Supergraph

1. `gap init` creates a starter gateway and hashes API keys with BLAKE3 (`INV-15`).
2. `gap config check` and `gap doctor` fail closed before serving.
3. `/readyz` exposes snapshot readiness (`generation: 1`).
4. Cross-branch queries resolve OpenAPI Objects (`{ id name }`) alongside MCP `JSON` scalars (`customer_notes(customer_id: 1)`) in one HTTP request.
""",
    }

    # 5. COURSE 2: MCP, AG-UI & AI ASSISTANTS
    pages["course-mcp-agents"] = {
        "lead": (
            "Course 2 shows you how AI assistants interact with hundreds of capabilities without overflowing their context window—using MCP 2026-07-28 progressive discovery (`L0 -> L1 -> L2`) and AG-UI v1.0 live SSE streaming."
        ),
        "learn_bullets": [
            "Compare `203,421`-token flat `tools/list` bloat vs. GAP's fixed `651`-token 5-tool MCP surface.",
            "Execute `gap_describe` (`L0`/`L2`), authorized `gap_search` (`L1`, `INV-05`), and `gap_query` step by step.",
            "Configure `--stdio` host clients (Claude/Cursor/Antigravity) and inspect AG-UI `v1.0` `/ag-ui` SSE events.",
        ],
        "sections": [
            {
                "id": "mcp-lesson-1",
                "h2": "Lesson 1: Stopping 203,421-Token Prompt Bloat (`tools/list` = 651 Tokens)",
                "subheadings": [],
                "html": _course_banner(
                    "mcp",
                    "Course 2: MCP & AI Assistants",
                    "20 min",
                    [
                        ("mcp-lesson-1", "651-Token Surface"),
                        ("mcp-lesson-2", "L0 -> L1 -> L2"),
                        ("mcp-lesson-3", "--stdio Hosts"),
                        ("mcp-lesson-4", "AG-UI SSE"),
                    ],
                )
                + """<p>In standard flat MCP integrations, connecting 100 microservices means <code>tools/list</code> returns ~1,000 raw JSON schemas totaling <strong>203,421 tokens</strong> on every cold turn. GAP exposes an invariant catalog of <strong>5 static tools costing 651 tokens</strong> from <code>N=10</code> to <code>N=1,000</code>:</p>"""
                + svg_progressive_levels("en")
                + lesson_footer(
                    "mcp-1",
                    [
                        "GAP holds `tools/list` constant at 5 static tools (`651` tokens) regardless of upstream branch count.",
                        "Discovery becomes explicit three-level on-demand hydration (`L0`, `L1`, `L2`).",
                    ],
                    "en",
                ),
            },
            {
                "id": "mcp-lesson-2",
                "h2": "Lesson 2: Driving Progressive Discovery (`L0 -> L1 -> L2 -> gap_query`)",
                "subheadings": [],
                "html": """<p>An AI agent moves through three deterministic hops—filtering out forbidden tools via the Cedar visibility bitset <strong>before</strong> ranking scores (<code>INV-05</code>):</p>"""
                + code_block(
                    '// L0 Overview (<= 24 tok/branch)\n{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"gap_describe","arguments":{"overview":true}}}\n\n// L1 Authorized Search\n{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"gap_search","arguments":{"query":"customer orders","limit":5}}}\n\n// L2 Hydrate Exact Compiled Types\n{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"gap_describe","arguments":{"types":["shop_CustomersResult","shop_OrdersResult"],"depth":1}}}',
                    "json",
                    "c2-l2-hop",
                    "en",
                    title="MCP 2026-07-28 — L0, L1, and L2 Discovery Sequence",
                )
                + code_challenge(
                    "c2-l2-chal",
                    "Request Exact Composed Types at L2",
                    "<p>Fill in the exact compiled OpenAPI return type name for Walking Skeleton customers inside <code>gap_describe</code>:</p>",
                    '{\n  "name": "gap_describe",\n  "arguments": { "types": ["____"], "depth": 1 }\n}',
                    ["shop_CustomersResult", '"shop_CustomersResult"'],
                    ["shop_CustomersResult", "shop_OrdersResult", "Customer"],
                    "In OpenAPI ingestion, branch_id \"shop\" plus the endpoint result type yields shop_CustomersResult (and shop_OrdersResult). Using bare Customer triggers a GAP_UNKNOWN_TYPE error.",
                    "en",
                )
                + lesson_footer(
                    "mcp-2",
                    [
                        "Use `gap_describe({overview: true})` at L0, `gap_search` at L1, and `gap_describe({types: [...]})` at L2.",
                        "Cedar visibility bitsets mask disallowed operations prior to BM25/HNSW scoring (`INV-05`).",
                    ],
                    "en",
                ),
            },
            {
                "id": "mcp-lesson-3",
                "h2": "Lesson 3: Connecting Desktop & IDE Agents via `--stdio` or HTTP",
                "subheadings": [],
                "html": """<p>Desktop hosts like Claude Desktop, Cursor, or Antigravity speak either newline-delimited <code>--stdio</code> JSON-RPC or Streamable HTTP <code>POST /mcp</code> (protocol header <code>MCP-Protocol-Version: 2026-07-28</code>). Configure <code>--stdio</code> cleanly while sending logs solely to <code>stderr</code>:</p>"""
                + code_block(
                    '{\n  "mcpServers": {\n    "gap-gateway": {\n      "command": "./target/dist/gap",\n      "args": ["serve", "--config", "examples/walking-skeleton/gap.toml", "--stdio"],\n      "env": { "GAP_TOKEN": "gap_dev_skeleton_key_01" }\n    }\n  }\n}',
                    "json",
                    "c2-l3-stdio",
                    "en",
                    title="Host MCP Configuration (--stdio)",
                )
                + lesson_footer(
                    "mcp-3",
                    [
                        "Under `--stdio`, stdout carries strictly valid JSON-RPC lines while structured logs write to stderr.",
                        "Set `MCP-Protocol-Version: 2026-07-28` when driving `POST /mcp` over HTTP/2 or HTTP/3.",
                    ],
                    "en",
                ),
            },
            {
                "id": "mcp-lesson-4",
                "h2": "Lesson 4: Streaming Interactive UI State with AG-UI `v1.0`",
                "subheadings": [],
                "html": """<p>Human-facing web copilot interfaces connect over <code>POST /ag-ui</code> Server-Sent Events (SSE), receiving typed <code>RUN_STARTED</code>, <code>TOOL_CALL_RESULT</code>, <code>STATE_SNAPSHOT</code>, interrupt prompts, and escaped <code>&lt;gap:untrusted&gt;</code> payloads:</p>"""
                + code_block(
                    'curl -sS -N http://127.0.0.1:8080/ag-ui \\\n  -H "Authorization: Bearer gap_dev_skeleton_key_01" \\\n  -H "Content-Type: application/json" \\\n  -d \'{"threadId":"t-1","runId":"r-1","messages":[{"id":"m-1","role":"user","content":"Show VIP customer 1"}],"state":{"generation":1}}\'',
                    "terminal",
                    "c2-l4-agui",
                    "en",
                    title="Terminal — Live AG-UI v1.0 SSE Run",
                )
                + practice_check(
                    "c2-q1",
                    "How does GAP prevent an unauthorized MCP agent from discovering even the existence of admin-only tools?",
                    [
                        "GAP applies the principal's Cedar visibility bitset BEFORE scoring BM25/HNSW hits (`INV-05`), excluding forbidden items from rankings and hit counts.",
                        "GAP relies on system prompt instructions asking the LLM not to call admin tools.",
                        "GAP runs tools first and hides error messages afterward.",
                    ],
                    0,
                    "Deterministic visibility filtering before ranking (INV-05) guarantees zero existence leakage.",
                    "en",
                )
                + lesson_footer(
                    "mcp-4",
                    [
                        "`POST /ag-ui` streams structured SSE events (`RUN_STARTED -> STATE_SNAPSHOT -> RUN_FINISHED`) with human-in-the-loop interrupts.",
                        "One gateway serves `/graphql`, `/mcp`, `/ag-ui`, and A2A `v1.0` over a single immutable Snapshot.",
                    ],
                    "en",
                ),
            },
        ],
        "markdown": """# Course 2: MCP, AG-UI & AI Assistants

1. Constant 5-tool MCP surface (`651` tokens) stops `203,421`-token prompt bloat.
2. Progressive flow: `L0 overview` -> `L1 gap_search` (`INV-05`) -> `L2 gap_describe(types: ["shop_CustomersResult"])` -> `gap_query`.
3. `--stdio` keeps stdout strictly JSON-RPC; HTTP uses `MCP-Protocol-Version: 2026-07-28`.
4. `POST /ag-ui` streams interactive SSE runs with untrusted data wrapped in `<gap:untrusted>`.
""",
    }

    # 6. COURSE 3: REST, SQL & N+1 BATCHING
    pages["course-connectors-batching"] = {
        "lead": (
            "Course 3 teaches federated query orchestration in GAP: diagnosing agent N+1 round-trip explosions, enabling declarative `x-gap-in-param` batching (`INV-18`), adding SQL & Code AST branches, and rolling out schemas safely via Propose-Not-Activate."
        ),
        "learn_bullets": [
            "Trace why an agent joining 50 customers without batching wastes `4` turns and `41,539`+ tokens (up to `814,959` at `N=1,000`).",
            "Declare OpenAPI `" + '"x-gap-in-param"' + "` batching (`INV-18`) and SQL parameterized `IN (...)` joins.",
            "Walk through `gap ingest once` -> `gap proposals list` -> `gap snapshot diff` -> `gap proposals approve`.",
        ],
        "sections": [
            {
                "id": "batch-lesson-1",
                "h2": "Lesson 1: Spotting the Agent N+1 Waterfall",
                "subheadings": [],
                "html": _course_banner(
                    "batch",
                    "Course 3: REST, SQL & N+1 Batching",
                    "25 min",
                    [
                        ("batch-lesson-1", "N+1 Waterfall"),
                        ("batch-lesson-2", "x-gap-in-param"),
                        ("batch-lesson-3", "SQL & Code AST"),
                        ("batch-lesson-4", "Safe Rollouts"),
                    ],
                )
                + """<p>When an LLM uses flat tools to fetch 50 customers and then retrieve orders or notes for each customer, the model re-reads the entire conversation transcript across 4 round-trips while emitting dozens of serialized HTTP requests. In <code>GAP-Bench</code>, at <code>N=1,000</code> tools this consumes <strong>814,959.8 tokens</strong> uncached vs. <strong>8,547.9 tokens</strong> (-99.0%) in GAP:</p>"""
                + svg_batch_join("en")
                + lesson_footer(
                    "batch-1",
                    [
                        "Flat agent orchestration turns relational joins into multi-turn prompt loops.",
                        "GAP deduplicates foreign keys inside `gap-exec` before touching upstream connectors.",
                    ],
                    "en",
                ),
            },
            {
                "id": "batch-lesson-2",
                "h2": "Lesson 2: Declarative REST Batching with `\"x-gap-in-param\"` (`INV-18`)",
                "subheadings": [],
                "html": """<p>Just like Apollo GraphOS REST Connectors batch keys with <code>$batch</code>, GAP's OpenAPI NGC adapter looks for <code>"x-gap-in-param"</code> on OpenAPI operations (verbatim from <code>examples/walking-skeleton/shop.openapi.json</code>):</p>"""
                + code_block(
                    '"/customers/{id}": {\n  "get": {\n    "operationId": "customer",\n    "x-gap-in-param": {\n      "path": "/customers",\n      "param": "ids",\n      "key_field": "id",\n      "max_batch_size": 50\n    }\n  }\n}',
                    "json",
                    "c3-l2-batch",
                    "en",
                    title="examples/walking-skeleton/shop.openapi.json",
                )
                + code_challenge(
                    "c3-x-gap",
                    "Configure OpenAPI Key Batching",
                    "<p>Fill in the exact OpenAPI vendor extension key checked by <code>gap-ngc-openapi</code> to collapse 50 individual lookups into bounded <code>ceil(unique_keys / max_batch_size)</code> calls (<code>INV-18</code>):</p>",
                    '"operationId": "customer",\n"____": {\n  "path": "/customers",\n  "param": "ids",\n  "key_field": "id",\n  "max_batch_size": 50\n}',
                    ["x-gap-in-param", '"x-gap-in-param"'],
                    ["x-gap-in-param", "x-gap-batch", "x-batch-keys"],
                    "Adding \"x-gap-in-param\" tells gap-ngc-openapi and gap-exec how to fold individual {id} fetches into /customers?ids=1,2,... bounded by max_batch_size (INV-18).",
                    "en",
                )
                + lesson_footer(
                    "batch-2",
                    [
                        "`INV-18` guarantees at most `ceil(unique_keys / max_batch)` RHS upstream calls per join edge.",
                        "No custom Rust glue code needed—just add `\"x-gap-in-param\"` in your OpenAPI JSON/YAML.",
                    ],
                    "en",
                ),
            },
            {
                "id": "batch-lesson-3",
                "h2": "Lesson 3: Unifying SQL Databases & `tree-sitter` Code ASTs (`208.7x`)",
                "subheadings": [],
                "html": """<p>Beyond REST, the same supergraph unifies relational databases (<code>gap-ngc-sql</code> using parameterized <code>IN (...)</code> clauses, never string concatenation) and source repositories (<code>gap-ngc-code</code> indexing callers, callees, and symbol definitions via <code>tree-sitter</code>):</p>"""
                + code_block(
                    '[[branches]]\nid = "codebase"\nprotocol = "code"\ntrust = "T0"\n\n[branches.code]\nroot = "."\nlanguages = ["rust", "python", "typescript"]',
                    "toml",
                    "c3-l3-code",
                    "en",
                    title="Example Code AST Branch Config (TOML)",
                )
                + """<p>On the <code>code_graph</code> benchmark (24 symbols), querying callers/callees through GAP replaces 11 noisy search-and-read agent turns (<code>137,810.4</code> cached tokens) with <strong>1 typed query (<code>660.4</code> tokens — a <code>208.7x</code> gain)</strong>.</p>"""
                + lesson_footer(
                    "batch-3",
                    [
                        "`gap-ngc-sql` projects foreign keys into batched joins using parameterized SQL `IN` clauses.",
                        "`gap-ngc-code` indexes repository AST graphs with `tree-sitter` for up to 208.7x code-understanding efficiency.",
                    ],
                    "en",
                ),
            },
            {
                "id": "batch-lesson-4",
                "h2": "Lesson 4: Safe Schema Updates (`Propose-Not-Activate` & `snapshot diff`)",
                "subheadings": [],
                "html": """<p>Neither schemas nor <code>@join</code> edges go live silently (<code>INV-06</code>). GAP runs sampled containment checks on proposed joins (never trusting LLM guesses) and requires operator inspection before atomic <code>ArcSwap</code> promotion:</p>"""
                + code_block(
                    "./target/dist/gap ingest once --config examples/walking-skeleton/gap.toml --tenant default\n./target/dist/gap proposals list\n./target/dist/gap snapshot diff --before examples/walking-skeleton/gap.toml --after next.toml\n./target/dist/gap proposals approve <proposal-id>",
                    "terminal",
                    "c3-l4-diff",
                    "en",
                    title="Terminal — Ingest, Diff & Approve Proposals Safely",
                )
                + practice_check(
                    "c3-q1",
                    "What happens if `gap ingest` discovers a candidate `@join` edge between two branches, but no operator has run `gap proposals approve`?",
                    [
                        "The candidate remains in `PROPOSED` state and is excluded from Plane 1 query planning (`INV-06`).",
                        "GAP automatically activates it after 30 seconds.",
                        "A neural router decides dynamically on each request whether to join it.",
                    ],
                    0,
                    "Only ACTIVE joins approved by an operator (after sampled key containment checks) may participate in query planning (INV-06).",
                    "en",
                )
                + lesson_footer(
                    "batch-4",
                    [
                        "Only `ACTIVE` `@join` edges execute on Plane 1 (`INV-06`).",
                        "`gap snapshot diff --before <old.toml> --after <new.toml>` previews every schema delta cleanly.",
                    ],
                    "en",
                ),
            },
        ],
        "markdown": """# Course 3: REST, SQL & N+1 Batching

1. N+1 agent loops waste up to `814,959` tokens; `INV-18` bounds calls to `ceil(unique_keys / max_batch)`.
2. Declare `"x-gap-in-param"` inside OpenAPI operations for REST batching.
3. Combine SQL (`IN (...)`) and `tree-sitter` Code AST (`208.7x` savings) behind one schema.
4. Roll out safely via `gap ingest once` -> `gap snapshot diff` -> `gap proposals approve` (`INV-06`).
""",
    }

    # 7. COURSE 4: ENTERPRISE ZERO-TRUST, CEDAR & HTTP/3
    pages["course-enterprise-zero-trust"] = {
        "lead": (
            "Course 4 prepares you to deploy GAP in adversarial production environments: isolating untrusted `T2` tools, enforcing two-person Cedar rules on `IRREVERSIBLE` mutations, stopping MCP rug pulls & SSRF, and serving dual-stack HTTP/3 QUIC with BLAKE3 audit verification."
        ),
        "learn_bullets": [
            "Enforce `T0/T1/T2` OS sandbox boundaries and `<gap:untrusted>` indirect prompt-injection containment.",
            "Write & verify Cedar rules in `policies/default.cedar` (`gap policy check --dir policies`).",
            "Inspect `DriftMonitor`, `EgressGuard` SSRF IP pinning, `0-RTT`-disabled HTTP/3 QUIC, and `gap audit verify`.",
        ],
        "sections": [
            {
                "id": "sec-lesson-1",
                "h2": "Lesson 1: Trust Tiers (`T0/T1/T2`) & `<gap:untrusted>` Escaping",
                "subheadings": [],
                "html": _course_banner(
                    "sec",
                    "Course 4: Zero-Trust, Cedar & HTTP/3",
                    "25 min",
                    [
                        ("sec-lesson-1", "T0/T1/T2 & Escaping"),
                        ("sec-lesson-2", "Cedar Policies"),
                        ("sec-lesson-3", "Rug Pull & SSRF"),
                        ("sec-lesson-4", "HTTP/3 & Audits"),
                    ],
                )
                + """<p>Every upstream branch sits in an explicit trust boundary: <code>T0</code> (Ed25519 organization-signed manifest; invalid signature demotes cleanly to <code>T2</code>), <code>T1</code> (admin-approved with JCS BLAKE3 hashes pinned), or <code>T2</code> (untrusted/external). All <code>T2</code> stdio processes require OS sandboxing (Bubblewrap/nsjail on Linux, Seatbelt on macOS—failing closed if absent) and cannot join across trust domains (<code>INV-07</code>).</p>
<p>Moreover, every payload leaving <code>T1/T2</code> branches is wrapped in <code>&lt;gap:untrusted&gt;...&lt;/gap:untrusted&gt;</code> with fake closing tags escaped (<code>SEC-080</code>) so upstream web/tool text can never hijack AI host instructions.</p>"""
                + lesson_footer(
                    "sec-1",
                    [
                        "Invalid Ed25519 manifest signatures demote a branch safely from `T0` to `T2`.",
                        "`T1`/`T2` outputs are automatically escaped inside `<gap:untrusted>` envelopes (`SEC-080`).",
                    ],
                    "en",
                ),
            },
            {
                "id": "sec-lesson-2",
                "h2": "Lesson 2: Two-Person Cedar Authorization Rules (`INV-04`, `INV-11`)",
                "subheadings": [],
                "html": """<p>In GAP, access control is never left to LLM judgment (<code>INV-04</code>). Cedar evaluates deterministically over <code>(principal, action, resource, context)</code> where <code>forbid</code> unconditionally beats <code>permit</code>. Look at the shipped rule in <code>policies/default.cedar</code>:</p>"""
                + code_block(
                    '@id("forbid-mutate-irreversible-or-unknown-unapproved")\nforbid (\n    principal,\n    action == Action::"Mutate",\n    resource\n) when {\n    (resource.sideEffect == "IRREVERSIBLE" || resource.sideEffect == "UNKNOWN")\n        && !(context.approved && context.approverIsDifferent)\n};\n\n# Validate syntax & types offline:\n./target/dist/gap policy check --dir policies',
                    "cedar",
                    "c4-l2-cedar",
                    "en",
                    title="policies/default.cedar & Validation CLI",
                )
                + code_challenge(
                    "c4-cedar-chal",
                    "Enforce Two-Person Approval in Cedar",
                    "<p>Complete the shipped Cedar guard so a caller cannot self-approve an <code>IRREVERSIBLE</code> mutation:</p>",
                    "when {\n  (resource.sideEffect == \"IRREVERSIBLE\" || resource.sideEffect == \"UNKNOWN\")\n    && !(context.approved && context.____)\n}",
                    ["approverIsDifferent"],
                    ["approverIsDifferent", "isAdmin", "mfaVerified"],
                    "Requiring context.approverIsDifferent prevents the requester's own token from acting as its own second approver.",
                    "en",
                )
                + lesson_footer(
                    "sec-2",
                    [
                        "Cedar `forbid` rules always override any `permit` statement.",
                        "Run `gap policy check --dir policies` in CI before reloading policies atomically with `gap policy reload`.",
                    ],
                    "en",
                ),
            },
            {
                "id": "sec-lesson-3",
                "h2": "Lesson 3: Stopping MCP Rug Pulls (`DriftMonitor`) & SSRF (`EgressGuard`)",
                "subheadings": [],
                "html": """<ul>
  <li><strong>Anti-Rug-Pull Pinning (<code>SEC-082</code>, <code>INV-08</code>):</strong> Before an upstream serves a modified tool description to sneak hidden prompt injections after admin review, <code>DriftMonitor</code> recomputes the JCS BLAKE3 <code>desc_hash</code>, detects mismatch, freezes the branch into <code>BranchStatus::Drift</code>, and continues serving only the pinned approved description.</li>
  <li><strong>Zero-Bypass SSRF Defense (<code>SEC-075</code>):</strong> When calling OpenAPI or webhook hosts, <code>EgressGuard</code> resolves DNS exactly once, blocks loopback/RFC1918/octal/hex/<code>169.254.169.254</code> metadata targets, pins the verified IP socket directly against DNS rebinding, and forbids HTTP redirects.</li>
</ul>"""
                + lesson_footer(
                    "sec-3",
                    [
                        "`DriftMonitor` prevents post-approval tool description tampering from ever reaching LLM agents.",
                        "`EgressGuard` blocks DNS rebinding, private ranges, cloud metadata IPs, and redirect pivots.",
                    ],
                    "en",
                ),
            },
            {
                "id": "sec-lesson-4",
                "h2": "Lesson 4: HTTP/3 QUIC, BLAKE3 Audit Ledger & Crash Recovery (`28.86 ms`)",
                "subheadings": [],
                "html": """<p>Ready for production deployment? Enable dual-stack HTTP/2 + HTTP/3 over UDP QUIC (which measured <strong>5,013 req/s</strong> at <code>2.36 ms</code> p95 with <strong>zero errors</strong> and recovered <code>/readyz</code> in <strong>28.86 ms</strong> after <code>SIGKILL</code>), with <code>max_early_data_size = 0</code> disabling <code>0-RTT</code> replay risks:</p>"""
                + svg_facade_flow("en")
                + code_block(
                    "./target/dist/gap audit verify --nightly\n./target/dist/gap audit export --format jsonld > prov-o-audit.json",
                    "terminal",
                    "c4-l4-audit",
                    "en",
                    title="Terminal — Verify BLAKE3 Chain & Export W3C PROV-O",
                )
                + practice_check(
                    "c4-q1",
                    "Why does GAP explicitly set `max_early_data_size = 0` (disabling QUIC TLS 1.3 `0-RTT`) across HTTP/3 listeners?",
                    [
                        "Because TLS 1.3 `0-RTT` packets are replayable on the network, which could double-execute `gap_mutate` operations.",
                        "Because QUIC does not support TLS 1.3 ALPN `h3` when `0-RTT` is off.",
                        "To force every client to downgrade back to HTTP/1.1.",
                    ],
                    0,
                    "Disabling 0-RTT guarantees replay immunity for every gateway mutation while preserving full multiplexed h3 throughput (5,013 req/s).",
                    "en",
                )
                + lesson_footer(
                    "sec-4",
                    [
                        "`profile = \"prod\"` fails closed unless explicit `tls_cert_secret` and `tls_key_secret` references are configured.",
                        "`gap audit verify --nightly` validates the tamper-evident BLAKE3 SQLite hash chain across mutations and approvals.",
                    ],
                    "en",
                ),
            },
        ],
        "markdown": """# Course 4: Zero-Trust, Cedar & HTTP/3

1. `T0` Ed25519 verification fails closed to `T2`; `T2` enforces OS sandboxing + `<gap:untrusted>`.
2. `policies/default.cedar` blocks `IRREVERSIBLE`/`UNKNOWN` mutations unless `context.approved && context.approverIsDifferent`.
3. `DriftMonitor` freezes rug-pull metadata drift (`INV-08`); `EgressGuard` stops DNS rebinding & metadata SSRF (`169.254.169.254`).
4. Dual-stack HTTP/2 + HTTP/3 QUIC disables `0-RTT` (`max_early_data_size = 0`) and records mutations in a BLAKE3 hash chain (`gap audit verify`).
""",
    }

    return pages
