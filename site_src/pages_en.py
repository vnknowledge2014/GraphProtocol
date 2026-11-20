"""English documentation pages 1..7 following Apollo GraphQL Docs pedagogical structure."""
from __future__ import annotations

from .diagrams import svg_mental_model, svg_three_planes
from .nav import link_to_asset, link_to_mirror, link_to_page
from .render import callout, code_block, data_table, esc, steps_list


def _ctx(slug: str):
    u = lambda target: link_to_page(slug, target, "en")
    a = lambda asset: link_to_asset(slug, "en", asset)
    m = link_to_mirror(slug, "en")
    return u, a, m


def build_en_part1() -> dict[str, dict]:
    pages: dict[str, dict] = {}

    # 1. HOME
    u, a, _ = _ctx("home")
    pages["home"] = {
        "lead": (
            "Graph Agent Protocol (GAP) is an open-source, typed GraphQL-profile gateway and zero-trust governance runtime "
            "built in Rust across 32 layered crates. Unify OpenAPI REST services, SQL databases, GraphQL subgraphs, gRPC services, "
            "CLI tools, codebases, MCP servers, and A2A peers behind one progressive interface."
        ),
        "learn_bullets": [
            "Connect heterogeneous tools without dumping 200k+ schema tokens into every LLM prompt.",
            "Eliminate agent N+1 round-trips using DataLoader join batching and wait-free ArcSwap snapshots.",
            "Enforce default-deny Cedar authorization, rug-pull description pinning, and SSRF egress guards.",
        ],
        "sections": [
            {
                "id": "get-started-paths",
                "h2": "Choose Your Starting Path",
                "subheadings": [],
                "html": f"""<p>Pick the path that fits your goal today. Each guide explains one focused concept cleanly before showing real commands from the repository.</p>
<div class="path-cards-grid">
  <a class="path-card" href="{esc(u('what-is-gap'))}">
    <span class="path-tag">Concept · 5 min</span>
    <h3>New to GAP? Start Here</h3>
    <p>Learn the 3 core problems GAP solves, how a supergraph organizes branches, and when Direct-MCP is still simpler.</p>
  </a>
  <a class="path-card" href="{esc(u('quickstart'))}">
    <span class="path-tag">Tutorial · 10 min</span>
    <h3>10-Minute Quickstart</h3>
    <p>Validate <code>gap.toml</code>, run the Walking Skeleton REST + MCP fixtures, and execute your first federated query.</p>
  </a>
  <a class="path-card" href="{esc(u('playground'))}">
    <span class="path-tag">Hands-On Lab</span>
    <h3>Interactive Lab &amp; Calculator</h3>
    <p>Test real <code>GAP-Bench</code> token tradeoffs, send live GraphQL/MCP requests via <code>serve.py</code>, and take the 6-question quiz.</p>
  </a>
</div>""",
            },
            {
                "id": "core-capabilities",
                "h2": "Explore Core Platform Capabilities",
                "subheadings": [],
                "html": f"""<div class="pillar-grid">
  <a class="pillar-card" href="{esc(u('architecture'))}">
    <h3>Three-Plane Architecture</h3>
    <p>Zero-ML hot execution (<code>15 ns</code> ArcSwap pinning, own pure-Rust AVL <code>OrdMap</code>) separated cleanly from discovery and control.</p>
  </a>
  <a class="pillar-card" href="{esc(u('progressive-disclosure'))}">
    <h3>Progressive Disclosure (651 Tokens)</h3>
    <p>Replace linear schema growth (<code>203,421</code> tokens at <code>N=1,000</code>) with constant 5-tool MCP discovery (<code>L0 -&gt; L1 -&gt; L2</code>).</p>
  </a>
  <a class="pillar-card" href="{esc(u('connectors-overview'))}">
    <h3>6 Native Graph Connectors</h3>
    <p>Declarative OpenAPI, SQL, GraphQL, gRPC, sandboxed CLI, and tree-sitter Code AST adapters gated by Propose-Not-Activate.</p>
  </a>
  <a class="pillar-card" href="{esc(u('facades-and-hosts'))}">
    <h3>MCP, A2A &amp; AG-UI Facades</h3>
    <p>Serve MCP <code>2026-07-28</code>, A2A <code>v1.0</code> durable tasks, and AG-UI <code>v1.0</code> <code>/ag-ui</code> SSE approval interrupts from one binary.</p>
  </a>
  <a class="pillar-card" href="{esc(u('zero-trust'))}">
    <h3>Zero-Trust Agent Governance</h3>
    <p>Cedar policies, <code>T0/T1/T2</code> isolation, <code>&lt;gap:untrusted&gt;</code> escaping, DriftMonitor rug-pull freeze, EgressGuard SSRF, and BLAKE3 audits.</p>
  </a>
  <a class="pillar-card" href="{esc(u('routers-and-http3'))}">
    <h3>HTTP/3 QUIC &amp; System-1 Routing</h3>
    <p>5,013 req/s multiplexed QUIC (<code>2.36 ms</code> p95, <code>0-RTT</code> off) alongside <code>95.24%</code> top-1 baseline routing and <code>INV-17</code> fallback.</p>
  </a>
</div>""",
            },
            {
                "id": "measured-highlights",
                "h2": "Measured Performance & Security Highlights",
                "subheadings": [],
                "html": f"""<p>Per <code>ADR-021</code>, every metric below comes directly from committed workspace test suites, <code>bench/results/gapbench/baseline.json</code>, and <code>bench/results/ga_local/report.json</code>.</p>
<div class="kpi-grid">
  <div class="kpi"><div class="val">651 tokens</div><div class="lbl">Constant MCP <code>tools/list</code> schema from N=10 to 1,000 tools (vs. 203,421 Direct-MCP)</div></div>
  <div class="kpi"><div class="val">99.0% / 208.7x</div><div class="lbl">Cross-branch join token savings at N=1,000 uncached &amp; 24-symbol code-graph efficiency</div></div>
  <div class="kpi"><div class="val">5,013 req/s</div><div class="lbl">HTTP/3 QUIC throughput (2.36 ms p95, 0 errors, 0-RTT off) alongside 1,064 req/s TCP HTTP/2</div></div>
  <div class="kpi"><div class="val">28.86 ms</div><div class="lbl">/readyz recovery after SIGKILL (kill -9) · 9.32 MiB idle RSS · 28.1 MB reproducible binary</div></div>
  <div class="kpi"><div class="val">87 / 87 WP</div><div class="lbl">100% specs traced (2,516 markers) · 136/136 conformance (CI) · 0 RUSTSEC advisories</div></div>
</div>
<div class="hero-actions">
  <a class="btn btn-primary" href="{esc(u('quickstart'))}">Follow the 10-Min Quickstart</a>
  <a class="btn" href="{esc(a('paper/gap-paper.html'))}" target="_blank" rel="noopener">Read Academic Paper (HTML)</a>
  <a class="btn" href="{esc(a('paper/gap-paper.pdf'))}" target="_blank" rel="noopener">Download Camera-Ready PDF</a>
  <a class="btn" href="https://github.com/vnknowledge2014/GraphProtocol" target="_blank" rel="noopener">GitHub Repository</a>
</div>""",
            },
            {
                "id": "launch-videos",
                "h2": "Product Walkthrough Videos (1080p)",
                "subheadings": [],
                "html": f"""<div class="lab-grid videos-grid">
  <div class="subcard">
    <h3 class="video-heading">Product Launch Overview (English Audio · 1080p)</h3>
    <video class="promo-video" controls preload="metadata" playsinline width="1920" height="1080">
      <source src="{esc(a('media/gap-promo-en.mp4'))}" type="video/mp4" />
      Your browser does not support MP4 playback (<a href="{esc(a('media/gap-promo-en.mp4'))}">download English MP4</a>).
    </video>
  </div>
  <div class="subcard">
    <h3 class="video-heading">Product Launch Overview (Vietnamese Audio · 1080p)</h3>
    <video class="promo-video" controls preload="metadata" playsinline width="1920" height="1080">
      <source src="{esc(a('media/gap-promo-vi.mp4'))}" type="video/mp4" />
      Your browser does not support MP4 playback (<a href="{esc(a('media/gap-promo-vi.mp4'))}">download Vietnamese MP4</a>).
    </video>
  </div>
</div>""",
            },
        ],
        "markdown": """# GAP Documentation Hub

Graph Agent Protocol (GAP) is an open-source (Apache-2.0), typed GraphQL-profile gateway and zero-trust governance layer for AI agents implemented across 32 layered Rust crates.

## Measured Highlights (ADR-021)
- Constant MCP `tools/list` schema: **651 tokens** from N=10 to 1,000 tools (vs. 203,421 in Direct-MCP)
- Token reduction at N=1,000 cross-branch joins: **99.0%** uncached (**93.5%** cached), turns 4 -> 2
- Code-graph efficiency (24 symbols): **119.32x** uncached / **208.68x** cached, turns 11 -> 1
- Dual-stack transport: **5,013 req/s** HTTP/3 QUIC (2.36 ms p95, 0-RTT disabled) & **1,064 req/s** TCP HTTP/2 (11.54 ms p95)
- Crash resilience & footprint: **28.86 ms** SIGKILL recovery, **9.32 MiB** idle RSS, **28.1 MB** bit-reproducible binary, **0** RUSTSEC advisories

## Start Here
- [What is GAP?](getting-started/what-is-gap.md)
- [10-Minute Quickstart](getting-started/quickstart.md)
- [Three-Plane Architecture](concepts/architecture.md)
- [Progressive Disclosure (651 Tokens)](concepts/progressive-disclosure.md)
""",
    }

    # 2. WHAT IS GAP?
    u, _, _ = _ctx("what-is-gap")
    pages["what-is-gap"] = {
        "lead": (
            "GAP gives AI agents one safe, typed entry point to call REST APIs, SQL databases, gRPC services, GraphQL subgraphs, CLIs, and code repositories—without stuffing thousands of JSON tool definitions into the LLM context window."
        ),
        "learn_bullets": [
            "Why loading flat tool catalogs directly into prompts breaks down as your system grows.",
            "Core GAP vocabulary: Gateway, Branch, Connector, Snapshot, and Principal.",
            "Exact crossover boundary: when to use Direct-MCP vs. when GAP dominates.",
        ],
        "sections": [
            {
                "id": "the-four-problems",
                "h2": "Why Flat Agent Tooling Hits a Wall",
                "subheadings": [],
                "html": """<p>Most agent integrations connect an LLM directly to individual Model Context Protocol (MCP) or HTTP endpoints. That works cleanly for 5–10 small tools, then fails in four predictable ways in production:</p>
<ul>
  <li><strong>Context Explosion:</strong> Serializing 1,000 flat JSON Schema tools up front consumes <strong>203,421 tokens</strong> before the user even asks a question.</li>
  <li><strong>N+1 Agent Waterfalls:</strong> Without a shared join planner, the model fetches a customer, loops through orders turn-by-turn, then queries notes separately—wasting latency and tokens.</li>
  <li><strong>Mutable Descriptions (Rug Pulls):</strong> If an untrusted upstream edits a tool description after approval to inject hidden instructions, flat clients forward the poisoned text straight to the LLM.</li>
  <li><strong>Probabilistic Authorization Leaks:</strong> Letting an LLM or neural classifier enforce access control introduces bypass risk. Access decisions must be deterministic code.</li>
</ul>""" + svg_mental_model("en"),
            },
            {
                "id": "key-terminology",
                "h2": "Five Terms You Need to Know",
                "subheadings": [],
                "html": data_table(
                    "Core Graph Agent Protocol concepts mapped cleanly",
                    ["Term", "Plain Meaning", "Real Counterpart in Repo"],
                    [
                        ["<code>gateway</code>", "Single HTTP/3 QUIC, HTTP/2, or stdio server binary that agents call.", "<code>gap-cli</code> / <code>gap-server</code>"],
                        ["<code>branch</code>", "One upstream backend source (an API, DB, CLI, codebase, or MCP server).", "Configured via <code>[[branches]]</code> in <code>gap.toml</code>"],
                        ["<code>connector (NGC)</code>", "Adapter projecting any backend into one typed protobuf + GraphQL schema.", "6 shipped adapters under <code>adapters/gap-ngc-*</code>"],
                        ["<code>snapshot</code>", "Immutable compiled bundle of schemas, indexes, and Cedar rules pinned per request.", "<code>gap-snapshot</code> (<code>ArcSwap</code>)"],
                        ["<code>principal</code>", "Authenticated human, agent, or service caller evaluated by Cedar default-deny policy.", "<code>gap-sec</code> + <code>policies/default.cedar</code>"],
                    ],
                ),
            },
            {
                "id": "when-not-to-use",
                "h2": "When Direct-MCP Is Still Simpler (Honest Crossover)",
                "subheadings": [],
                "html": callout(
                    "tradeoff",
                    "Transparent Benchmark Result (ADR-021)",
                    f"""<p>If your agent has <strong>50 or fewer simple read tools</strong> and your LLM provider keeps <strong>0.10x prompt prefix caching</strong> warm, Direct-MCP uses fewer tokens on single-step lookups (<code>547.2</code> vs. <code>4,691.9</code> tokens at <code>N=10</code>).</p>
<p>Adopt GAP immediately once you exceed ~200 tools, need cross-backend joins, navigate code ASTs, or require zero-trust security guarantees (Cedar approvals, SSRF EgressGuard, rug-pull freeze). Explore exact numbers in the <a href="{esc(u('playground'))}">Crossover Calculator</a>.</p>""",
                ),
            },
        ],
        "markdown": """# What is GAP?

GAP gives AI agents one safe, typed entry point to REST APIs, SQL databases, gRPC services, GraphQL subgraphs, CLIs, and code repositories without prompt bloat.

## Core Terminology
- **Gateway**: Single Rust binary serving `/graphql`, `/mcp`, `/ag-ui`, and A2A `v1.0`.
- **Branch**: Registered upstream source (`shop`, `notes`, `billing_sql`, etc.).
- **Connector (NGC)**: Translates external protocols into one typed GraphQL profile.
- **Snapshot**: Immutable compiled supergraph + search indexes + Cedar policy bundle.
- **Principal**: Authenticated caller evaluated by default-deny Cedar rules.

## Honest Tradeoff (ADR-021)
At N <= 50 flat lookups with prompt caching warm, Direct-MCP uses fewer raw tokens. GAP dominates once N >= 200, across multi-branch joins, code-graph traversal, or zero-trust deployments.
""",
    }

    # 3. QUICKSTART
    u, _, _ = _ctx("quickstart")
    pages["quickstart"] = {
        "lead": (
            "Follow these four steps to run the Walking Skeleton supergraph—combining a REST OpenAPI store (`shop`) and an MCP server (`notes`)—and query both in one typed request."
        ),
        "learn_bullets": [
            "Validate configuration files fail-closed with `gap config check` and `gap doctor`.",
            "Boot the local REST + MCP sample fixtures alongside `gap serve`.",
            "Send your first cross-branch GraphQL query joining both upstream sources.",
        ],
        "sections": [
            {
                "id": "prerequisites",
                "h2": "Prerequisites",
                "subheadings": [],
                "html": """<ul>
  <li>Built <code>gap</code> binary (from release assets or <code>./dist/build-reproducible.sh --offline</code> at <code>./target/dist/gap</code>).</li>
  <li>Python 3.10+ (for the built-in sample REST and MCP fixtures under <code>examples/walking-skeleton/fixtures/</code>).</li>
</ul>""" + callout(
                    "tip",
                    "Zero-Setup Browser Shortcut",
                    f"<p>Already running <code>python3 web/serve.py --port 8787</code>? You can boot these exact fixtures with one click inside the <a href=\"{esc(u('playground'))}\">Interactive Lab</a>.</p>",
                ),
            },
            {
                "id": "step-by-step",
                "h2": "Step-by-Step Setup",
                "subheadings": [],
                "html": steps_list([
                    (
                        "Inspect and validate the configuration",
                        """<p>Run static validation against <code>examples/walking-skeleton/gap.toml</code> before starting listeners. Unknown keys, unreferenced inline secrets, or missing spec files fail closed immediately:</p>"""
                        + code_block(
                            "./target/dist/gap config check examples/walking-skeleton/gap.toml\n./target/dist/gap doctor --config examples/walking-skeleton/gap.toml",
                            "terminal",
                            "qs-check",
                            "en",
                        )
                        + """<ul><li><strong>Check output:</strong> Both commands exit <code>0</code> (<code>OPS-044</code>). Notice the <code>dev-agent</code> API key uses a SHA/BLAKE3 hash—no plaintext secret is ever stored in TOML.</li></ul>""",
                    ),
                    (
                        "Start both upstream fixtures",
                        """<p>Open two terminal tabs (or run in background) to launch the mock REST API on port <code>9101</code> and the MCP server on port <code>9102</code>:</p>"""
                        + code_block(
                            "python3 examples/walking-skeleton/fixtures/rest_fixture.py --port 9101 &\npython3 examples/walking-skeleton/fixtures/mcp_fixture.py --port 9102 &",
                            "terminal",
                            "qs-fixtures",
                            "en",
                        ),
                    ),
                    (
                        "Boot the GAP Gateway",
                        """<p>Start <code>gap serve</code> pointing at the walking-skeleton configuration:</p>"""
                        + code_block(
                            "./target/dist/gap serve --config examples/walking-skeleton/gap.toml",
                            "terminal",
                            "qs-serve",
                            "en",
                        )
                        + """<ul><li><strong>Readiness probe:</strong> <code>GET http://127.0.0.1:8080/readyz</code> returns HTTP <code>200</code> as soon as snapshot generation <code>1</code> is pinned.</li></ul>""",
                    ),
                    (
                        "Execute your first cross-branch query",
                        """<p>Fetch customer profile and order rows from OpenAPI <code>shop</code> alongside agent notes from MCP <code>notes</code> in one call:</p>"""
                        + code_block(
                            'curl -sS -X POST http://127.0.0.1:8080/graphql \\\n  -H "Authorization: Bearer gap_dev_skeleton_key_01" \\\n  -H "Content-Type: application/json" \\\n  -d \'{"query": "{ customer(id: 1) { id name tier } orders(customer_id: 1) { id total_cents status } customer_notes(customer_id: 1) { notes { note_id summary } } }"}\' | python3 -m json.tool',
                            "terminal",
                            "qs-curl",
                            "en",
                        ),
                    ),
                ]),
            },
            {
                "id": "connect-ai-host",
                "h2": "Step 5: Connect Claude Desktop or Antigravity via MCP",
                "subheadings": [],
                "html": f"""<p>Rather than exposing every REST endpoint individually, attach your AI client to GAP's constant 5-tool MCP <code>2026-07-28</code> stdio server. Read full host patterns on <a href="{esc(u('facades-and-hosts'))}">MCP, A2A &amp; AG-UI Facades</a>.</p>"""
                + code_block(
                    '{\n  "mcpServers": {\n    "gap-gateway": {\n      "command": "/path/to/GraphProtocol/target/dist/gap",\n      "args": ["serve", "--config", "examples/walking-skeleton/gap.toml", "--stdio"]\n    }\n  }\n}',
                    "json",
                    "qs-mcp-cfg",
                    "en",
                ),
            },
        ],
        "markdown": """# 10-Minute Quickstart

Run the Walking Skeleton supergraph combining a REST OpenAPI backend (`shop`) and an MCP backend (`notes`).

## 1. Validate Configuration
```bash
./target/dist/gap config check examples/walking-skeleton/gap.toml
./target/dist/gap doctor --config examples/walking-skeleton/gap.toml
```

## 2. Start Upstream Fixtures & Gateway
```bash
python3 examples/walking-skeleton/fixtures/rest_fixture.py --port 9101 &
python3 examples/walking-skeleton/fixtures/mcp_fixture.py --port 9102 &
./target/dist/gap serve --config examples/walking-skeleton/gap.toml
```

## 3. Run Cross-Branch Query
Query `customer`, `orders` (OpenAPI) and `customer_notes` (MCP) in one HTTP call to `POST /graphql`.
""",
    }

    # 4. ARCHITECTURE
    u, _, _ = _ctx("architecture")
    pages["architecture"] = {
        "lead": (
            "GAP guarantees sub-microsecond snapshot swaps and deterministic security by strictly prohibiting machine-learning dependencies on its execution plane (`ARC-002`)."
        ),
        "learn_bullets": [
            "How Plane 1 (Execution), Plane 2 (Discovery), and Plane 3 (Control) isolate responsibilities.",
            "Why every request stays pinned to one immutable snapshot via `ArcSwap` and pure-Rust AVL `OrdMap`.",
            "Explore every component interactively using the 10-Node Architecture Inspector.",
        ],
        "sections": [
            {
                "id": "three-planes-diagram",
                "h2": "Strict Plane Separation",
                "subheadings": [],
                "html": svg_three_planes("en")
                + """<p>Notice the hard compiler boundary enforced by <code>tools/check_layers.py</code>: crates in the hot execution plane (<code>gap-exec</code>, <code>gap-plan</code>, <code>gap-transport</code>, <code>gap-snapshot</code>, <code>gap-balance</code>) fail CI immediately if any ML crate ever enters their dependency tree.</p>""",
            },
            {
                "id": "interactive-inspector",
                "h2": "Interactive 10-Node Architecture Explorer",
                "subheadings": [],
                "html": """<p>Select any block below to inspect its exact invariants, measured latency, and owning Rust source modules:</p>
<div id="planes-grid" class="planes-grid"></div>
<div id="arch-inspector" class="inspector-panel" aria-live="polite"></div>""",
            },
            {
                "id": "lock-free-snapshots",
                "h2": "Wait-Free Request Pinning & Memory Control",
                "subheadings": [],
                "html": """<ul>
  <li><strong>Zero Lock Across <code>.await</code> (INV-02):</strong> Inbound requests execute one atomic <code>ArcSwap::load</code> (~15 ns) and hold that immutable <code>Arc&lt;Snapshot&gt;</code> until completion.</li>
  <li><strong>Owned Pure-Rust AVL <code>OrdMap</code>:</strong> Replaced the unmaintained <code>im</code> crate (<code>RUSTSEC-2026-0097</code> ecosystem risk), eliminating 100% of <code>unsafe</code> blocks in snapshot state structures and delivering <code>0</code> RUSTSEC advisories.</li>
  <li><strong>Automatic 60s RollbackWatcher (INV-16):</strong> Publication swaps pre-warm top-100 plans, drop old snapshots on a background thread, and revert automatically if error rates spike across 200+ samples.</li>
</ul>""",
            },
        ],
        "markdown": """# Three-Plane Architecture

GAP divides gateway execution into three non-overlapping planes enforced in CI via `tools/check_layers.py`:
1. **Plane 1 — Deterministic Execution (Zero-ML, `ARC-002`):** AuthN, Cedar default-deny policies, tenant-keyed `PlanCache`, `ArcSwap` + pure-Rust AVL `OrdMap` immutable snapshots, `DataLoader` batching, `EgressGuard` SSRF defense.
2. **Plane 2 — Progressive Discovery:** L0 `__metaGraph`, L1 `__search` (visibility-filtered BM25 + HNSW), L2 `__type`, plus optional calibrated System-1 routers with deterministic `BASELINE_FALLBACK` (`INV-17`).
3. **Plane 3 — Zero-Trust Control:** Propose-Not-Activate ingestion, sampled `@join` data gates, `DriftMonitor` `desc_hash` rug-pull freeze, and auto `RollbackWatcher`.
""",
    }

    # 5. PROGRESSIVE DISCLOSURE
    u, _, _ = _ctx("progressive-disclosure")
    pages["progressive-disclosure"] = {
        "lead": (
            "Instead of streaming up to 203,421 JSON schema tokens into the prompt upfront, GAP holds MCP `tools/list` at a fixed 651 tokens and reveals schemas across three progressive levels (`L0 -> L1 -> L2`)."
        ),
        "learn_bullets": [
            "How Level 0 (`__metaGraph`), Level 1 (`__search`), and Level 2 (`__type`) work step-by-step.",
            "Why permission filtering happens BEFORE search scoring (`INV-05`).",
            "Inspect real payload sizes returned at each discovery hop.",
        ],
        "sections": [
            {
                "id": "three-levels-walkthrough",
                "h2": "Walkthrough: From Intent to Typed Query",
                "subheadings": [],
                "html": steps_list([
                    (
                        "L0 — Branch Overview (`__metaGraph` / `gap_describe`)",
                        "<p>The agent first requests a concise capability directory capped at &lt;= 24 tokens per branch:</p>"
                        + code_block(
                            'query { __metaGraph { id kind opCount summary } }',
                            "graphql",
                            "pd-l0",
                            "en",
                        )
                        + """<ul><li><strong>Returned summary:</strong> <code>shop (openapi, 4 ops)</code>, <code>notes (mcp, 1 op)</code>—hundreds of branches stay legible in a few hundred tokens.</li></ul>""",
                    ),
                    (
                        "L1 — Authorized Hybrid Search (`__search` / `gap_search`)",
                        "<p>The agent searches top-k matching operations using hybrid BM25 + HNSW vectors. GAP applies the caller's Cedar visibility bitset <strong>first</strong> (<code>INV-05</code>), so forbidden operations never appear in candidate lists or token counts:</p>"
                        + code_block(
                            'query { __search(query: "customer order notes", limit: 5) { branch operation kind score } }',
                            "graphql",
                            "pd-l1",
                            "en",
                        ),
                    ),
                    (
                        "L2 — On-Demand Type Hydration (`__type`)",
                        "<p>Only after choosing operations does the agent inspect the exact GraphQL input and return fields needed to build its query:</p>"
                        + code_block(
                            'query { __type(name: "Customer") { name fields { name type { name kind } } } }',
                            "graphql",
                            "pd-l2",
                            "en",
                        ),
                    ),
                ]),
            },
            {
                "id": "visibility-before-rank",
                "h2": "Security Guarantee: Visibility Filtering Before Ranking (INV-05)",
                "subheadings": [],
                "html": callout(
                    "important",
                    "No Existence Leakage",
                    "<p>If a restricted tenant or unprivileged agent queries <code>__search</code>, restricted branches and operations are masked out via the per-principal visibility bitset <em>before</em> scoring, ranking, or returning total counts.</p>",
                ),
            },
        ],
        "markdown": """# Progressive Disclosure (651 Tokens)

GAP keeps MCP `tools/list` fixed at 651 tokens regardless of whether 10 or 1,000 upstream tools exist:
- **L0 (`__metaGraph`):** Compact branch directory (<= 24 tokens/branch).
- **L1 (`__search`):** Visibility-filtered BM25 + HNSW search (`INV-05`).
- **L2 (`__type`):** Hydrate exact SDL types only for chosen operations.
""",
    }

    # 6. GAP-QL & JOINS
    u, _, _ = _ctx("gap-ql-and-joins")
    pages["gap-ql-and-joins"] = {
        "lead": (
            "GAP-QL compiles strictly valid GraphQL parsed by `apollo-compiler` and eliminates N+1 client round-trips with bounded `DataLoader` key batching (`INV-18`)."
        ),
        "learn_bullets": [
            "Why modeling discovery as `__metaGraph`/`__search`/`__type` preserves GraphQL parser compatibility.",
            "How GAP deduplicates join keys to dispatch `ceil(unique_keys / max_batch)` upstream calls (`INV-18`).",
            "Full `GAP-Bench` token matrix across `N in {10, 50, 200, 1000}`.",
        ],
        "sections": [
            {
                "id": "valid-graphql-profile",
                "h2": "Zero Custom Keywords (`GQL-001`)",
                "subheadings": [],
                "html": """<p>Instead of inventing non-standard GraphQL syntax like <code>traverse { ... }</code>, GAP exposes discovery and federation via standard schema root fields and validated directives (<code>@join</code>, <code>@sideEffect</code>, <code>@requiresScope</code>). Every query passes <code>apollo-compiler</code> validation and differential testing against <code>graphql-js 16.13.2</code>.</p>"""
                + code_block(
                    "directive @sideEffect(level: SideEffectLevel!) on FIELD_DEFINITION\ndirective @join(rhs: String!, key: String!, batchParam: String!, maxBatch: Int = 64) on FIELD_DEFINITION",
                    "graphql",
                    "gql-dirs",
                    "en",
                ),
            },
            {
                "id": "dataloader-batching",
                "h2": "Preventing N+1 Waterfalls (`INV-18`)",
                "subheadings": [],
                "html": f"""<p>Suppose a cross-branch query fetches 50 customers and joins their notes from an external MCP or SQL branch. Flat RPC loops fire 50 separate network requests. GAP extracts unique keys, enforces bounded chunks, and sends at most <code>ceil(unique_keys / max_batch)</code> <code>BatchQuery</code> frames—validated on every CI run (<code>VER-040</code>).</p>"""
                + data_table(
                    "GAP-Bench Measured Tokens (Uncached vs 0.10x Prompt Cache, N=10..1,000)",
                    ["Suite / Scale N", "Direct-MCP (Uncached)", "GAP (Uncached)", "Direct-MCP (Cached)", "GAP (Cached)", "Turns (Direct -> GAP)"],
                    [
                        ["<code>cross_branch_join</code> · N=10", "8,891.8", "<strong>8,517.9</strong>", "<strong>2,200.5</strong>", "7,383.7", "4 -> 2"],
                        ["<code>cross_branch_join</code> · N=50", "41,539.8", "<strong>8,547.9</strong>", "<strong>6,740.7</strong>", "7,413.7", "4 -> 2"],
                        ["<code>cross_branch_join</code> · N=200", "163,619.8", "<strong>8,547.9</strong>", "23,717.4", "<strong>7,413.7</strong>", "4 -> 2"],
                        ["<code>cross_branch_join</code> · N=1,000", "814,959.8", "<strong>8,547.9 (-99.0%)</strong>", "114,294.4", "<strong>7,413.7 (-93.5%)</strong>", "4 -> 2"],
                        ["<code>code_graph</code> · 24 symbols", "141,681.3", "<strong>1,187.4 (119.3x)</strong>", "137,810.4", "<strong>660.4 (208.7x)</strong>", "11 -> 1"],
                    ],
                ),
            },
        ],
        "markdown": """# GAP-QL & Batched Joins

GAP-QL is a strict GraphQL profile parsed by `apollo-compiler` and differentially tested against `graphql-js 16.13.2`.
- Eliminates N+1 loops: joins dispatch at most `ceil(unique_keys / max_batch)` upstream frames (`INV-18`).
- At `N=1,000` cross-branch joins, GAP reduces tokens by **99.0%** uncached and **93.5%** cached while halving agent turns (`4 -> 2`).
- On `code_graph` (24 symbols), GAP achieves **119.3x** (uncached) to **208.7x** (cached) token savings (`11 -> 1` turn).
""",
    }

    # 7. CONNECTORS OVERVIEW
    u, _, _ = _ctx("connectors-overview")
    pages["connectors-overview"] = {
        "lead": (
            "Native Graph Connectors (NGC) let you project existing REST APIs, SQL databases, gRPC services, GraphQL subgraphs, CLIs, and AST codebases into one unified graph without hand-writing glue resolvers."
        ),
        "learn_bullets": [
            "Walk through connecting an OpenAPI REST endpoint declaratively using `x-gap-in-param`.",
            "Compare all 6 shipped NGC adapters and their batching/sandbox behavior.",
            "How Propose-Not-Activate validates joins before any live traffic sees a new schema.",
        ],
        "sections": [
            {
                "id": "rest-to-gap-walkthrough",
                "h2": "How a REST API Becomes a Graph Branch",
                "subheadings": [],
                "html": """<p>Given an upstream REST endpoint <code>GET /orders?customer_id=...</code> or batch endpoint <code>GET /orders?customer_ids=1,2,3</code> returning JSON:</p>"""
                + code_block(
                    '{\n  "orders": [\n    { "id": 101, "customer_id": 1, "total_cents": 4200, "status": "paid" }\n  ]\n}',
                    "json",
                    "ngc-json",
                    "en",
                )
                + """<p>Declare the branch in <code>gap.toml</code> and mark the OpenAPI parameter <code>x-gap-in-param</code> in your OpenAPI spec so the executor automatically batches multi-customer joins:</p>"""
                + code_block(
                    '[[branches]]\nid = "shop"\nprotocol = "openapi"\ntrust = "T1"\n\n[branches.openapi]\nspec = "shop.openapi.json"\nbase_url = "http://127.0.0.1:9101"',
                    "toml",
                    "ngc-toml",
                    "en",
                )
                + """<ul>
  <li><strong><code>id = "shop"</code>:</strong> Stable branch identifier scoping generated GraphQL fields and audit logs.</li>
  <li><strong><code>trust = "T1"</code>:</strong> Admin-approved branch whose OpenAPI <code>spec_hash</code> and operation <code>desc_hash</code> are cryptographically pinned against silent mutations.</li>
</ul>""",
            },
            {
                "id": "six-connectors",
                "h2": "All 6 Shipped NGC Adapters",
                "subheadings": [],
                "html": data_table(
                    "Capabilities of the 6 reference Native Graph Connectors",
                    ["Adapter Crate", "Backend Source", "Batching & Safety Mechanism"],
                    [
                        ["<code>gap-ngc-openapi</code>", "OpenAPI 3.0 / REST JSON", "Supports <code>x-gap-in-param</code> key batching + <code>EgressGuard</code> IP pinning."],
                        ["<code>gap-ngc-sql</code>", "SQLite / PostgreSQL schemas", "Projects foreign keys into joinable graph edges with <code>IN (...)</code> parameterized batches."],
                        ["<code>gap-ngc-graphql</code>", "Existing GraphQL subgraphs", "Forwards typed sub-selections via <code>apollo-compiler</code> validation."],
                        ["<code>gap-ngc-grpc</code>", "gRPC / Protobuf services", "Pure-Rust <code>protox</code> compilation with TLS/mTLS support and deadline propagation."],
                        ["<code>gap-ngc-cli</code>", "Local CLI executables", "Runs strictly under Bubblewrap/Seatbelt OS sandbox with allowlisted env vars."],
                        ["<code>gap-ngc-code</code>", "Source codebases", "Uses <code>tree-sitter</code> AST indexing (calls, imports, symbols) for 208x token savings."],
                    ],
                ),
            },
            {
                "id": "propose-not-activate",
                "h2": "Safe Activation: Propose-Not-Activate & @join Verification",
                "subheadings": [],
                "html": """<p>Newly ingested schemas never auto-publish directly into request execution. Instead, <code>gap ingest once</code> compiles a Candidate Intermediate Schema Graph (CISG), runs sample data verification on proposed <code>@join</code> edges (checking containment &amp; null rates, never LLM guesswork), and queues a Proposal for operator review:</p>"""
                + code_block(
                    "./target/dist/gap ingest once --config gap.toml --tenant default\n./target/dist/gap proposals list\n./target/dist/gap snapshot diff\n./target/dist/gap proposals approve <proposal-id>",
                    "terminal",
                    "ngc-propose",
                    "en",
                ),
            },
        ],
        "markdown": """# Native Graph Connectors (NGC)

All backends—OpenAPI, SQL, GraphQL, gRPC, CLI, and Code AST—implement `proto/gap/ngc/v1/ngc.proto` (`Describe`, `BatchQuery`, `Mutate`, `StreamEvents`, `Ping`).
- **Batching**: OpenAPI `x-gap-in-param` and SQL `IN (...)` eliminate N+1 fanout (`INV-18`).
- **Propose-Not-Activate**: `gap ingest` queues proposals and checks `@join` candidates against sampled data before an operator runs `gap proposals approve`.
""",
    }

    return pages
