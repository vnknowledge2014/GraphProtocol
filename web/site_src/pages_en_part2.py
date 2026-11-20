"""English documentation pages 8..13 (Agent Hosts, HTTP/3 + S1, Zero-Trust, Interactive Lab, CLI/Ops, Reference)."""
from __future__ import annotations

from .diagrams import svg_facade_flow
from .nav import link_to_asset, link_to_page
from .render import callout, code_block, data_table, esc


def _ctx(slug: str):
    u = lambda target: link_to_page(slug, target, "en")
    a = lambda asset: link_to_asset(slug, "en", asset)
    return u, a


def build_en_part2() -> dict[str, dict]:
    pages: dict[str, dict] = {}

    # 8. FACADES AND HOSTS
    u, _ = _ctx("facades-and-hosts")
    pages["facades-and-hosts"] = {
        "lead": (
            "One running GAP gateway simultaneously serves three open AI interoperability protocols—MCP 2026-07-28, Agent2Agent (A2A v1.0), and AG-UI v1.0 SSE—without asking existing host clients to rewrite their protocol stacks."
        ),
        "learn_bullets": [
            "How all 5 static MCP meta-tools map onto progressive GAP-QL discovery and guarded mutations.",
            "Expose an A2A Agent Card with durable SQLite task leases and cycle detection (`INV-20`).",
            "Stream structured AG-UI `v1.0` events and Cedar human-in-the-loop approval interrupts over SSE.",
        ],
        "sections": [
            {
                "id": "three-axis-flow",
                "h2": "Three-Axis Interoperability Stack",
                "subheadings": [],
                "html": svg_facade_flow("en")
                + """<p>Think of GAP's facades as compatibility translators in front of one core engine: vertical tool hosts speak MCP, peer agent networks speak A2A, and browser/copilots speak AG-UI—all sharing the exact same immutable snapshot and Cedar policy checks.</p>""",
            },
            {
                "id": "mcp-facade",
                "h2": "Vertical Tool Facade: MCP 2026-07-28 (`651` Tokens)",
                "subheadings": [],
                "html": data_table(
                    "Five static MCP meta-tools exposed by adapters/gap-mcp",
                    ["Tool Name", "Behavior", "Policy / Safety Effect"],
                    [
                        ["<code>gap_search</code>", "Hybrid BM25 + HNSW search across active branches (<code>L1</code>).", "Pre-filtered by Cedar visibility bitset (<code>INV-05</code>)."],
                        ["<code>gap_describe</code>", "Branch overview (<code>L0</code>) or exact SDL type hydration (<code>L2</code>).", "Serves only approved <code>desc_hash</code> text (<code>INV-08</code>)."],
                        ["<code>gap_query</code>", "Executes read-only batched <code>GAP-QL</code> queries.", "Enforces read-only annotation + PlanCache <code>TenantId</code> key."],
                        ["<code>gap_mutate</code>", "Executes state-changing mutations.", "Requires <code>@sideEffect</code> + Cedar two-person approval when irreversible."],
                        ["<code>gap_task</code>", "Submits, polls, or cancels long-running A2A tasks.", "Persists FSM transitions in SQLite before emission (<code>INV-13</code>)."],
                    ],
                )
                + """<p>Configure Claude Desktop, Cursor, or Antigravity using either stdio mode (<code>gap serve --stdio</code>) or Streamable HTTP (<code>POST /mcp</code> with header <code>MCP-Protocol-Version: 2026-07-28</code>):</p>"""
                + code_block(
                    '{\n  "mcpServers": {\n    "gap-gateway": {\n      "command": "/usr/local/bin/gap",\n      "args": ["serve", "--config", "/etc/gap/gap.toml", "--stdio"]\n    }\n  }\n}',
                    "json",
                    "host-mcp-stdio",
                    "en",
                ),
            },
            {
                "id": "a2a-and-agui",
                "h2": "Horizontal A2A v1.0 & Frontend AG-UI v1.0",
                "subheadings": [],
                "html": """<ul>
  <li><strong>A2A v1.0 Peer Binding (<code>adapters/gap-a2a</code>):</strong> Publishes <code>/.well-known/agent-card.json</code> and handles durable 8-state asynchronous task lifecycles with monotonic hop counters that fail closed on delegation loops (<code>GAP_LOOP_DETECTED</code>, <code>INV-20</code>).</li>
  <li><strong>AG-UI v1.0 Event Streaming (<code>adapters/gap-agui</code>):</strong> Exposes <code>POST /ag-ui</code> SSE streaming typed events (<code>RunStarted</code>, <code>StepStarted</code>, <code>TextMessage*</code>, <code>ToolCall*</code>, <code>StateDelta</code>, <code>RunFinished</code>, <code>RunError</code>). When a Cedar policy requires human confirmation on an <code>IRREVERSIBLE</code> mutation, AG-UI pauses via a native <code>RUN_FINISHED</code> interrupt and resumes deterministically after approval.</li>
</ul>""",
            },
        ],
        "markdown": """# MCP, A2A & AG-UI Facades

GAP simultaneously exposes:
1. **MCP `2026-07-28` (`gap-mcp`):** 5 static tools (`gap_search`, `gap_describe`, `gap_query`, `gap_mutate`, `gap_task`) at 651 tokens over HTTP or `--stdio`.
2. **A2A `v1.0` (`gap-a2a`):** `/.well-known/agent-card.json`, SQLite durable task leases (`INV-13`), and cycle prevention (`INV-20`).
3. **AG-UI `v1.0` (`gap-agui`):** `POST /ag-ui` SSE streams, compact `StateDelta`, human-in-the-loop interrupts, and escaped `<gap:untrusted>` envelopes.
""",
    }

    # 9. ROUTERS & HTTP/3
    u, _ = _ctx("routers-and-http3")
    pages["routers-and-http3"] = {
        "lead": (
            "GAP decouples optional System-1 neural decision models (`@receptron/laya`, TypeSafe `Jev`, Google `gemini-decision-1`) from the deterministic runtime, and delivers dual-stack HTTP/2 + native HTTP/3 over QUIC with `0-RTT` disabled against replay attacks."
        ),
        "learn_bullets": [
            "Why GAP ships in `BASELINE_FALLBACK` routing mode rather than uncalibrated neural weights (`G-S1`).",
            "Configure optional `[router]` decision providers with egress allowlists and secret redaction.",
            "Enable native HTTP/3 QUIC (`RFC 9114`) with fail-closed `prod` TLS verification.",
        ],
        "sections": [
            {
                "id": "baseline-vs-s1",
                "h2": "System-1 Decision Routing & Gate `G-S1`",
                "subheadings": [],
                "html": """<p>Small neural routers only help if they beat fast lexical+structural ranking cleanly. Evaluated via <code>routebench</code> on held-out and debiased splits (<code>docs/decisions/G-S1.md</code>):</p>"""
                + data_table(
                    "RouteBench Measured Accuracy and Latency (G-S1)",
                    ["Router Implementation", "Held-Out / Test Top-1", "Debiased Top-1", "p95 Latency", "Default Shipping Mode"],
                    [
                        ["<strong>Baseline (BM25 + Structural)</strong>", "<strong>97.22% / 95.24%</strong>", "Measured baseline", "<strong>0.887 ms</strong>", "<strong>Enabled (Default)</strong>"],
                        ["<code>laya-candle:f32</code> (zero-shot)", "Below G-S1 gate", "14.29%", "73.85 ms", "Opt-in via <code>[router]</code> (<code>INV-17</code>)"],
                    ],
                )
                + callout(
                    "important",
                    "INV-17 Fail-Open Safety",
                    "<p>If any configured external System-1 provider (<code>@receptron/laya</code>, TypeSafe <code>jev</code> <code>/v1/systemone</code>, or Google <code>gemini_decision_1</code>) times out, returns invalid output, or is turned off, discovery falls back seamlessly to the deterministic baseline router while authorization remains untouched.</p>",
                )
                + code_block(
                    '[router]\nprovider = "gemini_decision_1"   # none | laya_candle | laya_http | jev | gemini_decision_1\nendpoint = "https://generativelanguage.googleapis.com"\napi_key_secret = "env://GEMINI_API_KEY"\ntimeout_ms = 250',
                    "toml",
                    "rt-s1-toml",
                    "en",
                ),
            },
            {
                "id": "http3-dual-stack",
                "h2": "Dual-Stack HTTP/2 + HTTP/3 over QUIC (`OPS-005`, `ADR-026`)",
                "subheadings": [],
                "html": """<p>Configured cleanly in <code>gap.toml</code>, <code>gap-transport</code> serves HTTPS/TCP (<code>HTTP/1.1</code> + <code>HTTP/2</code> with <code>Alt-Svc: h3=":8443"; ma=86400</code>) and UDP QUIC (<code>quinn</code> + <code>h3</code>, TLS 1.3 ALPN <code>h3</code>) on the same runtime:</p>"""
                + code_block(
                    'profile = "prod"\n\n[server]\nlisten = "0.0.0.0:8443"\nhttp3_listen = "0.0.0.0:8443"\ntls_cert_secret = "file:///etc/gap/cert.pem"\ntls_key_secret = "file:///etc/gap/key.pem"',
                    "toml",
                    "rt-h3-toml",
                    "en",
                )
                + """<ul>
  <li><strong>Replay Immunity:</strong> <code>max_early_data_size = 0</code> disables QUIC TLS 1.3 <code>0-RTT</code> early data everywhere so replayed packets can never trigger mutations twice.</li>
  <li><strong>Fail-Closed <code>prod</code> Guard:</strong> Booting with <code>profile = "prod"</code> and <code>http3_listen</code> without explicit <code>tls_cert_secret</code> / <code>tls_key_secret</code> refuses to start; <code>tls_self_signed_dev = true</code> is permitted only in <code>dev</code>.</li>
  <li><strong>Measured Local Benchmark:</strong> <strong>5,013.1 req/s</strong> over multiplexed QUIC (<strong>1.626 ms</strong> p50 / <strong>2.364 ms</strong> p95 / <code>0</code> errors) alongside <strong>1,063.6 req/s</strong> TCP HTTP/1.1+2 (<code>11.544 ms</code> p95).</li>
</ul>""",
            },
        ],
        "markdown": """# System-1 Routers & HTTP/3 QUIC

## System-1 vs Baseline (`G-S1`, `INV-17`)
- Baseline lexical+structural router achieves **97.22%** held-out / **95.24%** test top-1 at **0.887 ms** p95 and ships as default.
- Optional `[router]` providers (`laya_candle`, `laya_http`, `jev`, `gemini_decision_1`) fail open to baseline on timeout or error (`INV-17`).

## HTTP/3 QUIC (`OPS-005`, `ADR-026`)
- Dual-stack TCP HTTP/1.1+2 (`Alt-Svc`) + UDP QUIC `h3` via `quinn` + `h3`.
- **0-RTT explicitly disabled** (`max_early_data_size = 0`); `profile = "prod"` enforces explicit TLS secret references.
""",
    }

    # 10. ZERO-TRUST SECURITY
    u, _ = _ctx("zero-trust")
    pages["zero-trust"] = {
        "lead": (
            "GAP treats every external upstream tool description, response payload, and network destination as potentially hostile—enforcing Cedar default-deny policies, desc_hash rug-pull pinning, SSRF egress guards, OS sandboxing, and tamper-evident BLAKE3 audit logs."
        ),
        "learn_bullets": [
            "Classify backends into `T0`, `T1`, and `T2` trust tiers and inspect Cedar two-person mutation policies.",
            "Prevent MCP rug pulls with `DriftMonitor` and prompt smuggling with `<gap:untrusted>` envelopes.",
            "Stop DNS-rebinding / cloud-metadata SSRF (`169.254.169.254`) using `EgressGuard`.",
        ],
        "sections": [
            {
                "id": "trust-tiers-cedar",
                "h2": "Trust Tiers & Cedar Access Rules (`INV-04`, `INV-11`)",
                "subheadings": [],
                "html": """<p>Every upstream branch sits in an explicit trust tier:</p>
<ul>
  <li><strong><code>T0</code> (Platform Signed):</strong> Verified Ed25519 <code>branch.manifest.json</code> signature rooted in configured organization public keys. Any signature failure immediately demotes the branch to <code>T2</code>.</li>
  <li><strong><code>T1</code> (Admin Approved):</strong> Reviewed upstream APIs pinned by canonical schema digest (<code>spec_hash</code>) and operation description hash (<code>desc_hash</code>).</li>
  <li><strong><code>T2</code> (Untrusted / External):</strong> Sandboxed via OS namespaces (<code>Bubblewrap</code> / <code>nsjail</code> on Linux, <code>Seatbelt</code> on macOS)—failing closed if no OS sandbox is present—and blocked from joining across trust domains (<code>INV-07</code>).</li>
</ul>
<p>Access decisions are pure functions evaluated in Cedar (<code>policies/default.cedar</code>) where <code>forbid</code> always beats <code>permit</code>:</p>"""
                + code_block(
                    'forbid (\n  principal,\n  action == Gap::Action::"mutate",\n  resource\n) when {\n  resource.sideEffect == "IRREVERSIBLE" &&\n  !(context has approverIsDifferent && context.approverIsDifferent == true)\n};',
                    "cedar",
                    "sec-cedar",
                    "en",
                )
                + """<ul>
  <li><strong>Zero model involvement:</strong> No LLM confidence score can relax Cedar enforcement (<code>INV-04</code>), and spoofed MCP annotations like <code>readOnlyHint: true</code> are ignored.</li>
</ul>""",
            },
            {
                "id": "anti-rugpull-ssrf",
                "h2": "Anti-Rug-Pull Pinning, Untrusted Envelopes & SSRF Defense",
                "subheadings": [],
                "html": data_table(
                    "Protocol-level security mechanisms verified in tests/security/",
                    ["Threat Vector", "GAP Control", "Deterministic Failure / Isolation Behavior"],
                    [
                        ["<strong>MCP Tool Rug Pull</strong>", "<code>DriftMonitor</code> (<code>SEC-082</code>, <code>INV-08</code>)", "Recomputes JCS BLAKE3 <code>desc_hash</code>; mismatch freezes branch in <code>BranchStatus::Drift</code> while serving only pinned text."],
                        ["<strong>Indirect Prompt Injection</strong>", "<code>&lt;gap:untrusted&gt;</code> (<code>SEC-080</code>)", "Wraps T1/T2 payloads and HTML-escapes any fake closing tags inside upstream output."],
                        ["<strong>SSRF / DNS Rebinding</strong>", "<code>EgressGuard</code> (<code>SEC-075</code>)", "Resolves DNS once, rejects loopback/RFC1918/octal/hex/<code>169.254.169.254</code>, pins IP socket, blocks redirects."],
                        ["<strong>Bearer / Secret Leaks</strong>", "<code>Secret&lt;T&gt;</code> (<code>INV-15</code>)", "Redacts secrets across snapshots, Debug formatting, logs, OTel traces, metrics, and error messages."],
                        ["<strong>Audit Tampering</strong>", "BLAKE3 Ledger (<code>INV-14</code>)", "Every mutation, approval, and policy reload appends to SQLite hash chain verified by <code>gap audit verify</code>."],
                    ],
                ),
            },
        ],
        "markdown": """# Zero-Trust Governance & Cedar

GAP enforces defense in depth across 17 protocol attack vectors (`spec/07-security.md`):
- **Cedar Default-Deny (`INV-04`, `INV-11`):** `forbid` always wins; `IRREVERSIBLE` mutations require `approverIsDifferent == true`.
- **Anti-Rug-Pull (`SEC-082`, `INV-08`):** `DriftMonitor` freezes modified upstream tools in `BranchStatus::Drift`.
- **EgressGuard (`SEC-075`):** Resolves DNS once, pins socket to safe public IPs, and rejects cloud metadata (`169.254.169.254`) and redirects.
- **Audit Ledger (`INV-14`):** Tamper-evident BLAKE3 chain exportable as W3C PROV-O JSON-LD.
""",
    }

    # 11. PLAYGROUND
    u, _ = _ctx("playground")
    pages["playground"] = {
        "lead": (
            "Explore measured `GAP-Bench` token crossover curves, run real GraphQL and MCP requests against your local `gap serve` instance, check off the 6-level hands-on curriculum, and verify your mastery with the 6-question quiz."
        ),
        "learn_bullets": [
            "Simulate Direct-MCP vs. GAP token counts across workloads and scales (`N=10..1,000`).",
            "Switch from simulation mode to a real local `gap serve` socket with `python3 web/serve.py`.",
            "Save your 18-step hands-on learning progress across sessions (shared across English & Vietnamese).",
        ],
        "sections": [
            {
                "id": "interactive-lab-suite",
                "h2": "Crossover Calculator & Live Protocol Playground",
                "subheadings": [],
                "html": """<div class="lab-grid">
  <div class="subcard">
    <h3>3A. Token Efficiency Calculator (Measured ADR-021 Data)</h3>
    <div class="control-row">
      <label for="calc-suite">Workload:</label>
      <select id="calc-suite">
        <option value="cross_branch_join">cross_branch_join (Federated multi-branch join)</option>
        <option value="api_lookup">api_lookup (Single flat API lookup)</option>
        <option value="code_graph">code_graph (Codebase AST navigation, 24 symbols)</option>
      </select>
    </div>
    <div class="control-row">
      <label for="calc-n">Catalog scale N:</label>
      <select id="calc-n">
        <option value="10">N = 10 tools (2 branches)</option>
        <option value="50">N = 50 tools (5 branches)</option>
        <option value="200">N = 200 tools (20 branches)</option>
        <option value="1000" selected>N = 1000 tools (100 branches)</option>
      </select>
      <label><input id="calc-cached" type="checkbox" checked /> Enable Prompt Cache (0.10x)</label>
    </div>
    <div id="calc-output"></div>
  </div>

  <div class="subcard">
    <h3>3B. GAP-QL &amp; MCP 2026-07-28 Playground</h3>
    <div class="control-row">
      <button id="btn-toggle-demo" type="button" class="btn btn-primary">Start Live Lab (gap serve)</button>
      <span id="demo-badge" class="pill">Simulation Mode</span>
    </div>
    <div class="control-row">
      <label for="play-scenario">Scenario:</label>
      <select id="play-scenario"></select>
    </div>
    <p id="play-note" class="section-lead"></p>
    <div><strong>Request to <code id="play-endpoint"></code>:</strong></div>
    <pre role="region" aria-label="Sample request payload" tabindex="0"><code id="play-request"></code></pre>
    <div class="control-row">
      <button id="btn-run-play" type="button" class="btn btn-primary">Run scenario</button>
      <span id="play-meta" class="section-lead"></span>
    </div>
    <div><strong>Response output:</strong></div>
    <pre role="region" aria-label="Response output" tabindex="0"><code id="play-response"></code></pre>
  </div>
</div>""",
            },
            {
                "id": "curriculum-checklist",
                "h2": "6-Level Curriculum (Novice to Master)",
                "subheadings": [],
                "html": """<p>Navigate left to right using arrow keys (<code>focusgroup</code>) or click any tab. Progress persists automatically in your browser.</p>
<div class="progress-box progress-inline">
  <div><strong>Checklist progress:</strong> <span id="progress-text">0 / 18</span> items</div>
  <div class="progress-track" role="progressbar" aria-valuemin="0" aria-valuemax="18" aria-valuenow="0" aria-label="Checklist progress" id="progress-bar-wrap">
    <div id="progress-fill" class="progress-fill"></div>
  </div>
</div>
<div id="levels-tablist" class="tablist" focusgroup="tablist nomemory" aria-label="6 GAP learning levels"></div>
<div id="levels-tabpanels" class="tabpanels"></div>""",
            },
            {
                "id": "knowledge-quiz",
                "h2": "6-Question Architecture & Security Quiz",
                "subheadings": [],
                "html": """<div id="quiz-container"></div>
<div id="quiz-score" class="pill">Current score: 0 / 6</div>""",
            },
        ],
        "markdown": """# Interactive Lab & Calculator

Use this interactive page (`lab/playground.html`) in the browser to:
- Explore `GAP-Bench` Direct-MCP vs. GAP token crossover (`N=10..1,000`, uncached & cached)
- Trigger live local `gap serve` requests via `python3 web/serve.py --port 8787`
- Track your 18-step 6-level hands-on checklist
- Test understanding with the 6-question Quiz
""",
    }

    # 12. CLI & OPS
    u, _ = _ctx("cli-and-ops")
    pages["cli-and-ops"] = {
        "lead": (
            "Operate GAP in production using one self-contained 28.1 MB binary exposing 15 deterministic subcommands, standard exit codes (`OPS-044`), 9 Prometheus alert runbooks, and 32 strictly layered crates."
        ),
        "learn_bullets": [
            "Browse all 15 `gap` CLI subcommands and run live `--help` locally via `serve.py`.",
            "Inspect all 9 Prometheus alert runbooks with accessible Expand/Collapse All controls.",
            "Filter all 32 Rust crates by dependency layer (`L0` through `L6` and `Dev`).",
        ],
        "sections": [
            {
                "id": "cli-and-runbooks",
                "h2": "15 CLI Subcommands & 9 Incident Runbooks",
                "subheadings": [],
                "html": """<p>Standard process exit codes (<code>OPS-044</code>): <code>0</code> success · <code>1</code> runtime error · <code>2</code> config/usage error · <code>3</code> policy/integrity failure · <code>4</code> not found.</p>
<div class="lab-grid">
  <div class="subcard">
    <h3>15 Subcommands of the <code>gap</code> Binary</h3>
    <div id="cli-list" class="accordion"></div>
  </div>
  <div class="subcard">
    <div class="accordion-header">
      <h3>9 Operational Runbooks (Prometheus Alerts)</h3>
      <button id="btn-toggle-all-runbooks" type="button" class="btn btn-sm" commandfor="runbooks-accordion" command="--toggle-all" aria-expanded="false">
        <span class="expand">Expand all</span>
        <span class="collapse">Collapse all</span>
      </button>
    </div>
    <section id="runbooks-accordion" class="accordion" aria-label="9 incident response runbooks"></section>
  </div>
</div>""",
            },
            {
                "id": "crate-layer-map",
                "h2": "32-Crate Rust Architecture Map (Layers L0-L6)",
                "subheadings": [],
                "html": """<p>Higher-layer crates only depend downward (enforced by <code>tools/check_layers.py</code>). Select a layer chip to filter modules:</p>
<div id="layer-chips" class="chip-group" role="group" aria-label="Filter crates by architectural layer"></div>
<div id="crates-grid" class="crates-grid"></div>""",
            },
        ],
        "markdown": """# CLI, Runbooks & 32 Crates

## Exit Codes (`OPS-044`)
- `0`: success
- `1`: runtime error
- `2`: config/usage error
- `3`: policy/integrity denial
- `4`: resource not found

Inspect all 15 `gap` subcommands (`serve`, `init`, `doctor`, `config`, `models`, `proposals`, `snapshot`, `policy`, `audit`, `calibrate`, `bench`, `ingest`, `tasks`, `purge`, `completions`), 9 runbooks, and 32 crates interactively in `reference/cli-and-ops.html`.
""",
    }

    # 13. INVARIANTS & GLOSSARY
    u, a = _ctx("invariants-and-glossary")
    pages["invariants-and-glossary"] = {
        "lead": (
            "Review all 20 testable architectural invariants (`INV-01..20`), GAP's ASD-STE100 controlled vocabulary, reproducibility scripts, and the full camera-ready academic paper."
        ),
        "learn_bullets": [
            "Audit all 20 architectural invariants mapped to workspace `spec:` tests.",
            "Follow ASD-STE100 controlled naming (`dist/check-ste100.py`) when writing docs or prompts.",
            "Download the camera-ready PDF, HTML paper, or copy the BibTeX citation.",
        ],
        "sections": [
            {
                "id": "invariants-matrix",
                "h2": "20 Architectural Invariants (`INV-01` – `INV-20`)",
                "subheadings": [],
                "html": """<div tabindex="0" role="region" aria-labelledby="cap-inv" class="table-wrapper">
  <table class="data-table inv-table">
    <caption id="cap-inv">20 normative invariants enforced automatically across the GAP workspace</caption>
    <thead>
      <tr>
        <th scope="col">ID</th>
        <th scope="col">Requirement</th>
        <th scope="col">Enforcing Plane / Crate</th>
        <th scope="col">Verification Status</th>
      </tr>
    </thead>
    <tbody id="inv-tbody"></tbody>
  </table>
</div>""",
            },
            {
                "id": "ste100-dictionary",
                "h2": "ASD-STE100 Controlled Terminology",
                "subheadings": [],
                "html": """<p>To prevent synonym drift in technical specs and LLM prompts, <code>dist/check-ste100.py</code> enforces a single approved term per concept and caps procedural sentences at 20 words:</p>
<div tabindex="0" role="region" aria-labelledby="cap-dict" class="table-wrapper">
  <table class="data-table dict-table">
    <caption id="cap-dict">Approved ASD-STE100 controlled terms in GAP</caption>
    <thead>
      <tr>
        <th scope="col">Approved Term</th>
        <th scope="col">GAP Definition</th>
        <th scope="col">Prohibited Synonyms</th>
      </tr>
    </thead>
    <tbody id="dict-tbody"></tbody>
  </table>
</div>""",
            },
            {
                "id": "paper-and-citation",
                "h2": "Academic Paper & Citation",
                "subheadings": [],
                "html": f"""<div class="hero-actions">
  <a class="btn btn-primary" href="{esc(a('paper/gap-paper.html'))}" target="_blank" rel="noopener">Read Academic Paper (HTML)</a>
  <a class="btn" href="{esc(a('paper/gap-paper.pdf'))}" target="_blank" rel="noopener">Download Paper PDF</a>
  <a class="btn" href="{esc(a('CITATION.cff'))}" target="_blank" rel="noopener">CITATION.cff</a>
</div>"""
                + code_block(
                    '@article{nguyen2026gap,\n  author  = {Nguy{\\~e}n, Duy {\\DH}{\\\'u}c},\n  title   = {Graph Agent Protocol (GAP): Unifying Typed Discovery, Batched Federation, and Zero-Trust Governance for AI Agents},\n  year    = {2026},\n  note    = {Reference Release v0.1.0-rc.1; Infinite Agent Labs; ORCID: 0009-0009-4741-5637},\n  url     = {https://github.com/vnknowledge2014/GraphProtocol}\n}',
                    "bibtex",
                    "ref-bibtex",
                    "en",
                ),
            },
        ],
        "markdown": """# 20 Invariants, ASD-STE100 & Academic Paper

- **20 Architectural Invariants (`INV-01..20`):** Enforced across 2,516 test markers via `tools/spec_trace.py --strict`.
- **ASD-STE100 Controlled Vocabulary:** Checked automatically by `python3 dist/check-ste100.py`.
- **Author:** Duy Đức Nguyễn (Infinite Agent Labs, ORCID `0009-0009-4741-5637`).
- **Repository:** `https://github.com/vnknowledge2014/GraphProtocol`
""",
    }

    return pages
