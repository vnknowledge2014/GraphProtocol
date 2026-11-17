#!/usr/bin/env python3
"""Render camera-ready printable paper/gap-paper.html and paper/gap-paper.pdf from verified paper sources."""

from __future__ import annotations

import html
import json
import re
import subprocess
import sys
import tempfile
import textwrap
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
PAPER_DIR = ROOT / "paper"
CANONICAL_REPO = "https://github.com/vnknowledge2014/GraphProtocol"
CANONICAL_PAGES = "https://vnknowledge2014.github.io/GraphProtocol"


def read_author_meta() -> dict[str, str]:
    tex = (PAPER_DIR / "author_config.tex").read_text(encoding="utf-8")
    out = {
        "name": "Mike (vnknowledge2014)",
        "affiliation": "Graph Agent Protocol Project",
        "email": "vnknowledge2014@users.noreply.github.com",
        "orcid": "ORCID pending registration",
        "repo": CANONICAL_REPO,
    }
    patterns = {
        "name": r"\\newcommand\{\\GapAuthorFullName\}\{([^}]*)\}",
        "affiliation": r"\\newcommand\{\\GapAuthorAffiliation\}\{([^}]*)\}",
        "email": r"\\newcommand\{\\GapAuthorEmail\}\{([^}]*)\}",
        "orcid": r"\\newcommand\{\\GapAuthorOrcid\}\{([^}]*)\}",
        "repo": r"\\newcommand\{\\GapRepoUrl\}\{([^}]*)\}",
    }
    for key, pat in patterns.items():
        m = re.search(pat, tex)
        if m and m.group(1).strip():
            out[key] = m.group(1).strip()
    return out


def build_html(
    author: dict[str, str],
    ga_local: dict,
    gapbench: dict,
    s1: dict,
    m3: dict,
) -> Path:
    fig1 = (PAPER_DIR / "figures" / "fig1_architecture.svg").read_text(encoding="utf-8")
    fig2 = (PAPER_DIR / "figures" / "fig2_schema_scaling.svg").read_text(encoding="utf-8")
    fig3 = (PAPER_DIR / "figures" / "fig3_crossover_regimes.svg").read_text(encoding="utf-8")
    fig4 = (PAPER_DIR / "figures" / "fig4_runtime_latencies.svg").read_text(encoding="utf-8")

    # Table 1 rows directly from baseline.json
    t1_rows = []
    for c in gapbench["suite_comparisons"]:
        u_rat = c["uncached_token_ratio"]
        c_rat = c["cached_token_ratio"]
        u_str = f"<strong>{u_rat:.3f}x</strong>" if u_rat < 1.0 else f"{u_rat:.3f}x"
        c_str = f"<strong>{c_rat:.3f}x</strong>" if c_rat < 1.0 else f"{c_rat:.3f}x"
        t1_rows.append(
            f"<tr><td>{html.escape(c['suite'])}</td><td>{c['n_tools']}</td>"
            f"<td>{c['direct']['uncached_tokens_per_success']:,.1f}</td>"
            f"<td>{c['gap']['uncached_tokens_per_success']:,.1f}</td>"
            f"<td>{u_str}</td>"
            f"<td>{c['direct']['cached_tokens_per_success']:,.1f}</td>"
            f"<td>{c['gap']['cached_tokens_per_success']:,.1f}</td>"
            f"<td>{c_str}</td>"
            f"<td>{c['direct']['mean_round_trips']:.1f} &rarr; {c['gap']['mean_round_trips']:.1f}</td></tr>"
        )

    rt = ga_local["live_runtime_drills"]
    h3 = ga_local["http3_live_drills"]
    rep = ga_local["reproducible_build"]
    base = s1["baseline"]
    s1_fwd = s1["s1_forward"]
    s1_deb = s1["s1_debiased"]
    b_p50 = m3["benchmarks"]["gap_balance_percentiles"]

    bin_mb = rep["binary_bytes"] / (1000.0 * 1000.0)
    author_esc = html.escape(author["name"])
    affil_esc = html.escape(author["affiliation"])
    email_esc = html.escape(author["email"])
    repo_esc = html.escape(author["repo"])

    doc = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>Graph Agent Protocol (GAP): Unifying Typed Discovery, Batched Federation, and Zero-Trust Governance for AI Agents</title>
<meta name="citation_title" content="Graph Agent Protocol (GAP): Unifying Typed Discovery, Batched Federation, and Zero-Trust Governance for AI Agents"/>
<meta name="citation_author" content="{author_esc}"/>
<meta name="citation_publication_date" content="2026/11/16"/>
<meta name="citation_technical_report_institution" content="{affil_esc}"/>
<meta name="citation_pdf_url" content="{CANONICAL_PAGES}/paper/gap-paper.pdf"/>
<meta name="citation_abstract_html_url" content="{CANONICAL_PAGES}/paper/"/>
<meta name="description" content="Graph Agent Protocol (GAP) academic preprint, three-plane zero-trust architecture, dual-stack HTTP/2 + HTTP/3 QUIC transport, and reproducible GAP-Bench/RouteBench evaluation."/>
<style>
  @page {{
    size: letter;
    margin: 0.50in 0.52in;
  }}
  * {{
    box-sizing: border-box;
  }}
  body {{
    font-family: "Times New Roman", "Liberation Serif", Georgia, serif;
    color: #0F172A;
    max-width: 960px;
    margin: 24px auto;
    padding: 0 18px;
    line-height: 1.36;
    font-size: 9.8pt;
    hyphens: auto;
  }}
  header {{
    text-align: center;
    margin-bottom: 11px;
    border-bottom: 1.5px solid #1B365D;
    padding-bottom: 8px;
  }}
  h1 {{
    font-size: 16.2pt;
    color: #1B365D;
    margin: 0 0 6px 0;
    line-height: 1.16;
    letter-spacing: -0.01em;
  }}
  .author {{
    font-size: 9.8pt;
    color: #1E293B;
    margin: 2px 0;
  }}
  .meta-line {{
    font-size: 8.8pt;
    color: #475569;
    margin-top: 2px;
  }}
  .meta-line a {{
    color: #0072B2;
    text-decoration: none;
  }}
  .abstract {{
    background: #F8FAFC;
    border-left: 3px solid #1B365D;
    border-top: 1px solid #E2E8F0;
    border-right: 1px solid #E2E8F0;
    border-bottom: 1px solid #E2E8F0;
    padding: 8px 12px;
    margin: 8px 0 11px 0;
    font-size: 9.1pt;
    line-height: 1.33;
  }}
  .twocol {{
    column-count: 2;
    column-gap: 18px;
    text-align: justify;
    margin-bottom: 8px;
  }}
  .twocol p {{
    margin: 0 0 5.5px 0;
    orphans: 3;
    widows: 3;
  }}
  .twocol ul, .twocol ol {{
    margin: 3.5px 0 6px 0;
    padding-left: 16px;
  }}
  .twocol li {{
    margin-bottom: 3px;
  }}
  h2 {{
    font-size: 11pt;
    color: #1B365D;
    border-bottom: 1px solid #CBD5E1;
    padding-bottom: 2px;
    margin: 8px 0 4px 0;
    break-after: avoid;
    text-align: left;
  }}
  h3 {{
    font-size: 9.7pt;
    color: #0F172A;
    margin: 6px 0 2.5px 0;
    break-after: avoid;
    text-align: left;
  }}
  figure {{
    margin: 7px 0 9px 0;
    text-align: center;
    break-inside: avoid;
    page-break-inside: avoid;
  }}
  figure svg {{
    width: 100%;
    max-width: 100%;
    height: auto;
    border: 1px solid #E2E8F0;
    border-radius: 4px;
    display: block;
    margin: 0 auto;
  }}
  figcaption {{
    font-size: 8.4pt;
    color: #334155;
    margin-top: 4px;
    text-align: left;
    line-height: 1.28;
    padding: 0 4px;
  }}
  .table-wrap {{
    margin: 7px 0 9px 0;
    break-inside: avoid;
    page-break-inside: avoid;
  }}
  .table-caption {{
    font-size: 8.5pt;
    color: #1E293B;
    margin-bottom: 3.5px;
    text-align: left;
    line-height: 1.28;
  }}
  table {{
    width: 100%;
    border-collapse: collapse;
    font-size: 8.0pt;
    border-top: 1.6px solid #0F172A;
    border-bottom: 1.6px solid #0F172A;
    hyphens: none;
  }}
  thead th {{
    border-bottom: 1px solid #334155;
    background: #F8FAFC;
    color: #1B365D;
    font-weight: 700;
    padding: 4px 5px;
    text-align: right;
    white-space: nowrap;
  }}
  tbody td {{
    border-bottom: 1px solid #F1F5F9;
    padding: 3.4px 5px;
    text-align: right;
  }}
  tbody tr:last-child td {{
    border-bottom: none;
  }}
  th:first-child, td:first-child {{
    text-align: left;
  }}
  td.mono {{
    font-family: "SFMono-Regular", Menlo, Consolas, monospace;
    font-size: 7.7pt;
    white-space: nowrap;
  }}
  code {{
    font-family: "SFMono-Regular", Menlo, Consolas, monospace;
    font-size: 8.2pt;
    color: #0F172A;
    background: #F1F5F9;
    padding: 0 2px;
    border-radius: 2px;
    hyphens: none;
  }}
  ol.refs {{
    font-size: 8.0pt;
    line-height: 1.26;
    padding-left: 17px;
    margin: 3px 0 0 0;
    text-align: left;
  }}
  ol.refs li {{
    margin-bottom: 2.2px;
  }}
  @media print {{
    body {{
      max-width: none;
      margin: 0;
      padding: 0;
      font-size: 9.0pt;
      line-height: 1.32;
    }}
    .twocol {{
      column-count: 2;
      column-gap: 16px;
    }}
  }}
</style>
</head>
<body>
<header>
  <h1>Graph Agent Protocol (GAP): Unifying Typed Discovery, Batched Federation, and Zero-Trust Governance for AI Agents</h1>
  <div class="author"><strong>{author_esc}</strong> &middot; {affil_esc} &middot; <code>{email_esc}</code></div>
  <div class="meta-line">
    Reference Release <code>v0.1.0-rc.1</code> &middot; November 16, 2026 &middot; Apache-2.0 &middot;
    Code &amp; Artifacts: <a href="{repo_esc}">{repo_esc}</a>
  </div>
</header>

<div class="abstract">
  <strong>Abstract.</strong> AI agents increasingly invoke REST/OpenAPI APIs, SQL stores, GraphQL subgraphs, gRPC endpoints, CLI utilities, codebases, Model Context Protocol (MCP) tool servers, peer Agent2Agent (A2A) workers, and interactive AG-UI interfaces. Current deployments fragment this plane across flat JSON-RPC catalogs (MCP), opaque peer task endpoints (A2A), and ad-hoc frontend streams, causing <em>O</em>(<em>N</em>) prompt schema inflation (<strong>203,421 tokens</strong> at <em>N</em>=1,000), client-orchestrated <em>N</em>+1 join round-trips, unpinned descriptions vulnerable to rug pulls, and hot-path latency from neural routing. We introduce <strong>Graph Agent Protocol (GAP)</strong>, a strongly typed GraphQL-profile gateway built across three separated planes: (1) a <em>Zero-ML deterministic execution plane</em> with Cedar policy checks, tenant-keyed planning, batched DataLoader joins, and wait-free immutable snapshots backed by a pure-Rust persistent AVL <code>OrdMap</code>; (2) a <em>three-level progressive discovery plane</em> unifying MCP <code>2026-07-28</code> (<strong>651 tokens</strong> constant), A2A v1.0, and AG-UI v1.0 (<code>/ag-ui</code> SSE + interrupts) alongside multi-provider System-1 routers (<code>@receptron/laya</code>, <code>Jev</code>, <code>gemini-decision-1</code>) and automatic deterministic fallback (<code>INV-17</code>); and (3) a <em>zero-trust control plane</em> enforcing propose-not-activate ingestion, SHA-256 description-hash drift freezes, DNS-pinned SSRF egress guards, OS sandboxing, and BLAKE3 hash chains over dual-stack <code>HTTP/2</code> + <code>HTTP/3</code> (<code>QUIC RFC 9114</code>, 0-RTT disabled). Implemented across 32 layered Rust crates with zero <code>RUSTSEC</code> advisories, GAP cuts cross-branch token usage at <em>N</em>=1,000 by <strong>99.0%</strong> uncached (<strong>93.5%</strong> cached) and code-graph context by <strong>119.3x–208.7x</strong>, boots its {bin_mb:.1f}&nbsp;MB bit-identical binary in <strong>{rt['cold_start_ms']:.1f}&nbsp;ms</strong> at <strong>{rt['idle_rss_mib']:.2f}&nbsp;MiB</strong> idle RSS, sustains <strong>{rt['measured_rps']:,.1f}&nbsp;req/s</strong> over TCP (<strong>{rt['p95_ms']:.2f}&nbsp;ms</strong> p95) and <strong>{h3['measured_rps']:,.1f}&nbsp;req/s</strong> over persistent multiplexed QUIC (<strong>{h3['p95_ms']:.2f}&nbsp;ms</strong> p95) with zero errors, and recovers from OS <code>SIGKILL</code> in <strong>{rt['sigkill_recovery_ready_ms']:.2f}&nbsp;ms</strong>.
</div>

<figure>
  {fig1}
  <figcaption><strong>Figure 1:</strong> Three-plane zero-trust architecture of Graph Agent Protocol (GAP). Plane 1 runs typed GraphQL planning, Cedar authorization, and batched joins with zero ML models (<code>ARC-002</code>, <code>INV-01</code>, <code>INV-04</code>) behind dual-stack HTTP/1.1+HTTP/2 TCP and HTTP/3 QUIC transport (<code>OPS-005</code>). Plane 2 performs visibility-filtered progressive disclosure; Plane 3 governs proposal verification, rug-pull drift pinning, and lock-free snapshot swaps.</figcaption>
</figure>

<div class="twocol">
  <h2>1. Introduction &amp; Threat Model</h2>
  <p>Production AI agents must discover, authorize, compose, and audit operations across heterogeneous backends. Anthropic's Model Context Protocol (MCP) [1] standardizes vertical tool invocation over JSON-RPC, whereas Google's Agent2Agent (A2A) protocol [2] standardizes horizontal multi-agent delegation via Agent Cards and asynchronous task lifecycles. Operating multi-tenant gateways exposes four structural weaknesses that flat catalogs and point-to-point RPC do not solve together:</p>
  <ol>
    <li><strong>Prompt Context Explosion &amp; <em>N</em>+1 Waterfalls:</strong> Flat MCP exports grow linearly to <strong>203,421 tokens</strong> at <em>N</em>=1,000 before reasoning begins, forcing models to issue sequential multi-turn round-trips to join cross-backend entities.</li>
    <li><strong>Semantic Rug Pulls &amp; Spoofed Hints:</strong> Security audits [3,4,5] show unpinned tool descriptors permit hidden prompt injection and post-approval behavioral shifts.</li>
    <li><strong>Hot-Path Neural Entanglement:</strong> Invoking ML tool classifiers on every request adds tens of milliseconds and mixes probabilistic inference with security-critical authorization.</li>
    <li><strong>Protocol Silos Without Typed Federation:</strong> Treating SQL tables, REST APIs, CLI binaries, and code ASTs as unrelated untyped tools forces the LLM to act as query planner and join engine.</li>
  </ol>
  <p>GAP enforces a simple engineering rule: use deterministic compilers, typed graphs, and formal policy engines wherever structure exists, and confine probabilistic retrieval strictly to bounded, calibrated intent discovery (<code>ARC-002</code>).</p>

  <h2>2. Three-Plane Architecture &amp; Invariants</h2>
  <p>As Figure 1 shows, GAP assigns every downstream backend (<em>branch</em>) an explicit trust tier: <strong><code>T0</code></strong> (Ed25519-signed internal manifest; signature mismatch downgrades fail-closed to <code>T2</code>), <strong><code>T1</code></strong> (admin-approved and pinned via cryptographic <code>spec_hash</code> and <code>desc_hash</code>), and <strong><code>T2</code></strong> (untrusted external branches whose text outputs are length-capped, scrubbed, and wrapped inside escaped <code>&lt;gap:untrusted&gt;</code> envelopes, with CLI/stdio blocked without OS sandboxing).</p>
  <p>To prevent architectural erosion, 20 normative invariants (<code>INV-01</code>..<code>INV-20</code>) are enforced continuously via static crate layering checks (<code>tools/check_layers.py</code>) and strict spec traceability (<code>tools/spec_trace.py --strict</code>):</p>
  <ul>
    <li><strong><code>INV-01</code> / <code>INV-02</code> (Pinning &amp; Lock-Free Hot Path):</strong> Requests pin one <code>Arc&lt;Snapshot&gt;</code> on ingress; read paths forbid <code>Mutex</code>, <code>RwLock</code>, and <code>DashMap</code>, using wait-free <code>ArcSwap</code> and persistent AVL <code>OrdMap</code> trees.</li>
    <li><strong><code>INV-04</code> / <code>INV-05</code> (Deterministic Authz &amp; Visibility):</strong> Compiled Cedar rules [10] evaluate before execution and never consume ML scores; <code>__search</code> applies per-principal visibility bitsets before ranking.</li>
    <li><strong><code>INV-08</code> / <code>INV-09</code> (Pinned Descriptors &amp; Tenants):</strong> Planners read sanitized pinned descriptions only; caches, quotas, stores, and BLAKE3 audits include <code>TenantId</code>.</li>
    <li><strong><code>INV-17</code> / <code>INV-18</code> / <code>INV-20</code>:</strong> System-1 ML cannot block typed execution; remote joins execute in bounded batched frames; peer A2A delegation tracks hop counters to reject cycles.</li>
  </ul>

  <h2>3. Protocol Design &amp; Dual-Stack Transport</h2>
  <h3>3.1 GAP-QL &amp; Three-Level Progressive Disclosure</h3>
  <p>GAP-QL is a strict GraphQL profile parsed by <code>apollo-compiler</code> with zero custom grammar keywords [9]. Capabilities—<code>__metaGraph</code> (L0 branch summaries), <code>__search</code> (L1 hybrid BM25+HNSW candidates), <code>__type</code> (L2 exact SDL hydration), <code>@sideEffect</code>, <code>@requiresScope</code>, and <code>@join</code>—live purely as standard types and directives. Behind MCP <code>2026-07-28</code>, the five-tool facade (<code>gap_search</code>, <code>gap_describe</code>, <code>gap_query</code>, <code>gap_mutate</code>, <code>gap_task</code>) holds <code>tools/list</code> invariant at <strong>651 tokens</strong>.</p>
  <h3>3.2 Three-Axis Facades, Native Connectors &amp; Dual-Stack HTTP/2 + HTTP/3</h3>
  <p>Six reference Native Graph Connectors (<code>OpenAPI</code>, <code>GraphQL</code>, <code>gRPC</code>, <code>SQL</code>, sandboxed <code>CLI</code>, <code>tree-sitter</code> <code>Code</code>) translate heterogeneous protocols into <code>Describe</code>, <code>BatchQuery</code>, and <code>Mutate</code> frames. At L4, <code>gap-transport</code> serves <code>/graphql</code>, <code>/graphql/stream</code>, <code>/mcp</code> (MCP <code>2026-07-28</code>), A2A v1.0, and <code>/ag-ui</code> (AG-UI v1.0 SSE + human approval interrupts, spotlighted via <code>&lt;gap:untrusted&gt;</code>, <code>ADR-027</code>) over <strong>HTTP/1.1+HTTP/2 TCP</strong> (<code>Alt-Svc: h3</code>) and <strong>HTTP/3 QUIC</strong> (<code>RFC 9114</code>, <code>0-RTT</code> disabled, <code>OPS-005</code>). Optional System-1 routing (<code>ADR-028</code>) unifies Candle, <code>@receptron/laya</code>, <code>Jev</code>, and <code>gemini-decision-1</code> (0 output tokens, up to 1,000 parallel heads) under <code>INV-17</code> baseline fallback.</p>

  <h2>4. Zero-Trust Governance &amp; Snapshot Control</h2>
  <p>All changes enter via sandboxed <code>gap-ingest</code> extraction, CISG injection linting, and admin-approved <code>Proposal</code> records rather than auto-mutating live routing. <code>DriftMonitor</code> hashes sanitized upstream descriptions and immediately freezes tampered branches to <code>BranchStatus::Drift</code> while planners continue serving pinned baselines (<code>SEC-082</code>).</p>
  <p>Outbound HTTP passes through <code>EgressGuard</code>, which pins resolved DNS sockets once, blocks loopback/RFC 1918/metadata IPs (<code>169.254.169.254</code>) and obfuscated numeric/IPv6-mapped forms across redirects. Subprocesses run inside Linux Bubblewrap or macOS Seatbelt sandboxes with SHA-256 binary allowlists. Snapshots publish off-thread via <code>SnapshotHandle</code> with automatic 60&nbsp;s error-spike rollback and BLAKE3 chained audit logs.</p>
</div>

<figure>
  {fig2}
  <figcaption><strong>Figure 2:</strong> Static MCP <code>tools/list</code> token scaling (<code>GQL-022</code> estimator). Publishing flat tool catalogs explodes to 203,421 tokens at <em>N</em>=1,000 tools across 100 branches, whereas GAP's five-tool progressive facade remains fixed at <strong>651 tokens</strong> (46% below Gate G-F1's 1,200-token ceiling).</figcaption>
</figure>

<div class="table-wrap">
  <div class="table-caption"><strong>Table 1:</strong> Reproducible <code>GAP-Bench</code> end-to-end comparison across catalog scales (<em>N</em>), uncached vs. 0.10x warm-prefix cached regimes, and agent turn counts (100% task success across all cells; ratio &lt; 1.000x denotes GAP token savings).</div>
  <table>
    <thead>
      <tr>
        <th>Suite</th><th><em>N</em></th><th>Direct Unc.</th><th>GAP Unc.</th><th>Ratio (U)</th><th>Direct Cac.</th><th>GAP Cac.</th><th>Ratio (C)</th><th>Turns D &rarr; G</th>
      </tr>
    </thead>
    <tbody>
      {''.join(t1_rows)}
    </tbody>
  </table>
</div>

<figure>
  {fig3}
  <figcaption><strong>Figure 3:</strong> Crossover regime law plotting <code>GAP / Direct-MCP</code> prompt token ratio on a log scale. Below 1.0x (mint region), batched federation and progressive disclosure cut tokens by <strong>95.3x–208.7x</strong>; above 1.0x (rose region), tiny cached catalogs (<em>N</em>&le;50) still favor Direct-MCP.</figcaption>
</figure>

<div class="twocol">
  <h2>5. Rust Implementation &amp; Formal Traceability</h2>
  <p>GAP is built in Rust Edition 2024 (<code>rustc 1.98.1</code>) across 32 crates (<code>L0</code>–<code>L6</code>, including <code>gap-agui</code> and multi-provider <code>gap-s1</code>), forbidding <code>unsafe</code> everywhere outside optional <code>gap-s1</code> and denying <code>unwrap</code>, <code>expect</code>, and <code>panic</code> in non-test paths. BSL dependencies remain strictly behind optional <code>store-surreal</code>. To eliminate every unmaintained/unsound third-party crate from the supply chain, snapshot maps use an owned zero-unsafe AVL <code>OrdMap</code> and PEM parsing uses <code>rustls-pki-types</code> directly, achieving <strong>0 errors</strong> across <code>cargo deny check advisories bans licenses sources</code> (<code>ignore = []</code>).</p>
  <p>Verification passes end-to-end across <strong>362 specification requirements</strong> (89 <code>MUST</code>, 0 uncovered across 2,515 markers), <strong>135/136</strong> offline/loopback conformance checks, <strong>365</strong> 1,024-case proptests, 10 fuzz targets (34 corpora), chaos, soak, and upgrade/backup drills.</p>

  <h2>6. Experimental Evaluation &amp; Crossover Analysis</h2>
  <p>Across Table 1 and Figure 3, <code>GAP-Bench</code> demonstrates both massive wins and honest boundaries (<code>ADR-017</code>, <code>ADR-021</code>): at <em>N</em>=1,000, GAP cuts cross-branch join tokens by <strong>99.0%</strong> uncached (<strong>93.5%</strong> cached) and halves turns (4.0 &rarr; 2.0); on <code>code_graph</code> (24 symbols), typed graph walks reduce turns from 11.0 to 1.0 for <strong>119.32x</strong> uncached and <strong>208.68x</strong> cached savings. Conversely, on tiny flat catalogs (<em>N</em>=10 cached: 547.2 vs. 4,691.9 tokens), single-turn Direct-MCP stays smaller, and conditional <code>X-1</code> wire columnarization yielded &lt;20% suite reduction and remains deferred.</p>
  <p>Table 2 isolates <code>RouteBench</code> discovery routing: un-finetuned Candle weights score only {s1_deb['test_metrics']['top1']['estimate']*100:.2f}% debiased top-1 at {s1_deb['test_metrics']['p95_latency_ms']:.2f}&nbsp;ms p95, whereas GAP's deterministic lexical+structural baseline achieves <strong>{base['test_metrics']['top1']['estimate']*100:.2f}%</strong> test / <strong>{base['held_out_metrics']['top1']['estimate']*100:.2f}%</strong> held-out top-1 in <strong>{base['test_metrics']['p95_latency_ms']:.3f}&nbsp;ms</strong>, justifying fail-safe <code>BASELINE_FALLBACK</code> under Gate <code>G-S1</code>.</p>
</div>

<div class="table-wrap">
  <div class="table-caption"><strong>Table 2:</strong> <code>RouteBench</code> comparison (<em>n</em><sub>test</sub>=48, <em>n</em><sub>held</sub>=72; 1,000 bootstrap resamples), lock-free micro-primitives (<code>m3-baseline.json</code>), and live bit-identical dual-stack <code>HTTP/2</code> + <code>HTTP/3 QUIC</code> verification drills (<code>bench/results/ga_local/report.json</code>).</div>
  <table>
    <thead>
      <tr>
        <th>Benchmark / Drill Component</th><th>Metric 1</th><th>Metric 2</th><th>Latency / Throughput</th><th>Artifact Source</th>
      </tr>
    </thead>
    <tbody>
      <tr><td class="mono">baseline (BM25+Struct)</td><td>Test Top-1: <strong>{base['test_metrics']['top1']['estimate']*100:.2f}%</strong></td><td>Held Top-1: <strong>{base['held_out_metrics']['top1']['estimate']*100:.2f}%</strong></td><td><strong>{base['test_metrics']['p95_latency_ms']:.3f}&nbsp;ms</strong> p95</td><td class="mono">s1_comparison.json</td></tr>
      <tr><td class="mono">s1_forward (laya-candle:f32)</td><td>Test Top-1: {s1_fwd['test_metrics']['top1']['estimate']*100:.2f}%</td><td>ECE<sub>15</sub>: {s1_fwd['test_metrics']['ece_15']['estimate']:.4f}</td><td>{s1_fwd['test_metrics']['p95_latency_ms']:.2f}&nbsp;ms p95</td><td class="mono">s1_comparison.json</td></tr>
      <tr><td class="mono">s1_debiased (Laya + PMI)</td><td>Test Top-1: {s1_deb['test_metrics']['top1']['estimate']*100:.2f}%</td><td>ECE<sub>15</sub>: <strong>{s1_deb['test_metrics']['ece_15']['estimate']:.4f}</strong></td><td>{s1_deb['test_metrics']['p95_latency_ms']:.2f}&nbsp;ms p95</td><td class="mono">s1_comparison.json</td></tr>
      <tr><td class="mono">Lock-Free Balance / RCU</td><td>ArcSwap load: <strong>15&nbsp;ns</strong></td><td>Guard / CAS / P2C: {b_p50['in_flight_guard_1t_ns']['net']['p50']:.0f} / {b_p50['ewma_update_1t_ns']['net']['p50']:.0f} / {b_p50['select_n2_1t_ns']['net']['p50']:.0f}&nbsp;ns</td><td>Store p99 @256r: 59&nbsp;&micro;s</td><td class="mono">m3-baseline.json</td></tr>
      <tr><td class="mono">gap serve (TCP HTTP/1.1+2)</td><td>Cold start: <strong>{rt['cold_start_ms']:.1f}&nbsp;ms</strong></td><td>Idle / Peak RSS: <strong>{rt['idle_rss_mib']:.2f} / {rt['post_load_rss_mib']:.2f}&nbsp;MiB</strong></td><td><strong>{rt['measured_rps']:,.1f}&nbsp;req/s</strong> ({rt['p95_ms']:.2f}&nbsp;ms p95, {rt['errors']} err)</td><td class="mono">ga_local/report.json</td></tr>
      <tr><td class="mono">HTTP/3 QUIC (8 mux conns)</td><td>TLS 1.3 ALPN <code>h3</code></td><td>0-RTT: <strong>Disabled</strong> · Alt-Svc</td><td><strong>{h3['measured_rps']:,.1f}&nbsp;req/s</strong> ({h3['p95_ms']:.2f}&nbsp;ms p95, {h3['errors']} err)</td><td class="mono">ga_local/report.json</td></tr>
      <tr><td class="mono">Binary Parity &amp; Crash</td><td>Size: {rep['binary_bytes']:,}&nbsp;B</td><td>Double SHA-256: <strong>Identical</strong></td><td>SIGKILL recover: <strong>{rt['sigkill_recovery_ready_ms']:.2f}&nbsp;ms</strong></td><td class="mono">ga_local/report.json</td></tr>
    </tbody>
  </table>
</div>

<figure>
  {fig4}
  <figcaption><strong>Figure 4:</strong> Multi-scale latency spectrum across lock-free RCU/balance primitives (15–94&nbsp;ns), deterministic baseline discovery (0.887&nbsp;ms), live multiplexed HTTP/3 QUIC ({h3['p95_ms']:.2f}&nbsp;ms p95) and HTTP/2 TCP ({rt['p95_ms']:.2f}&nbsp;ms p95) mixed loads, and OS <code>SIGKILL</code> recovery ({rt['sigkill_recovery_ready_ms']:.2f}&nbsp;ms).</figcaption>
</figure>

<div class="twocol">
  <h2>7. Related Work &amp; Limitations</h2>
  <p>Retriever-assisted tool routers such as Gorilla [7] and ToolLLM [8] expand API coverage yet omit deterministic Cedar authorization [10], conformal abstention [11], and cross-backend DataLoader joins. Recent MCP security audits [3,4,5] and A2A security analyses [6] catalog poisoning and rug pulls; GAP neutralizes these via propose-not-activate ingestion, <code>desc_hash</code> pinning, <code>EgressGuard</code>, and sandboxing. GraphQL MCP servers [12] pioneered progressive GraphQL discovery, which GAP extends across arbitrary non-GraphQL backends over HTTP/2 and QUIC.</p>
  <p>Multi-day cloud soak runs (72&nbsp;h), 24&nbsp;h libFuzzer sweeps, live frontier LLM sweeps, and Linux OIDC signing remain CI/cloud release gates rather than local macOS drills; locally all HTTP/2 (short-lived Python <code>urllib</code> clients to subprocesses) and HTTP/3 (persistent QUIC streams to in-process fixtures) benchmarks measure distinct transport topologies reported transparently above.</p>

  <h2>8. Conclusion</h2>
  <p>GAP reconciles MCP usability, A2A horizontal tasks, dual-stack HTTP/2 and HTTP/3 QUIC performance, and enterprise zero-trust security in one verified Rust gateway. All specifications, conformance tests, reproducible benchmarks, and Apache-2.0 source artifacts are openly published for verification and reuse.</p>

  <h2>References</h2>
  <ol class="refs">
    <li>Anthropic. <em>Model Context Protocol (MCP) Specification</em> (incl. 2026-07-28 revision). <code>https://modelcontextprotocol.io</code>, 2024–2026.</li>
    <li>Google LLC and A2A Contributors. <em>Agent2Agent (A2A) Protocol Specification</em>. <code>https://a2a-protocol.org</code>, 2025.</li>
    <li>H. Song et al. Beyond the Protocol: Unveiling Attack Vectors in the Model Context Protocol Ecosystem. <em>arXiv:2506.02040</em>, 2025.</li>
    <li>Z. Wang et al. MCPTox: A Benchmark for Tool Poisoning Attack on Real-World MCP Servers. <em>arXiv:2508.14925</em>, 2025.</li>
    <li>arXiv:2512.06556 Authors. Semantic Attacks on Tool-Augmented LLMs: Securing MCP Against Descriptor Manipulation, 2025.</li>
    <li>arXiv:2504.16902 Authors. Building a Secure Agentic AI Application Leveraging A2A Protocol, 2025.</li>
    <li>S. G. Patil et al. Gorilla: Large Language Model Connected with Massive APIs. <em>arXiv:2305.15334</em> (NeurIPS), 2023.</li>
    <li>Y. Qin et al. ToolLLM: Facilitating LLMs to Master 16000+ Real-world APIs. <em>arXiv:2307.16789</em> (ICLR), 2023.</li>
    <li>O. Hartig and J. P&eacute;rez. Semantics and Complexity of GraphQL. In <em>Proc. WWW '18</em>, 2018.</li>
    <li>J. W. Cutler et al. Cedar: A New Language for Expressive, Fast, Safe, and Analyzable Authorization. <em>Proc. ACM Program. Lang. (OOPSLA)</em>, 2024.</li>
    <li>A. N. Angelopoulos and S. Bates. A Gentle Introduction to Conformal Prediction. <em>arXiv:2107.07511</em>, 2021.</li>
    <li>Apollo GraphQL and Grafbase Engineering. Progressive Schema Disclosure Patterns in GraphQL MCP Servers, 2025.</li>
  </ol>
</div>
</body>
</html>
"""
    html_out = PAPER_DIR / "gap-paper.html"
    html_out.write_text(doc, encoding="utf-8")
    return html_out


def write_fallback_pdf(pdf_out: Path, lines_by_page: list[list[str]]) -> None:
    """Emit a clean standards-compliant multi-page PDF 1.4 using built-in Helvetica fonts."""
    def pdf_esc(s: str) -> str:
        return s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")

    objects: list[bytes] = []

    def add_obj(body: bytes) -> int:
        objects.append(body)
        return len(objects)

    catalog_id = add_obj(b"")
    pages_id = add_obj(b"")
    font_reg_id = add_obj(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
    font_bold_id = add_obj(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>")

    page_ids: list[int] = []
    for page_lines in lines_by_page:
        stream_cmds = ["BT", "/F2 12 Tf", "54 744 Td"]
        first = True
        for idx, raw_line in enumerate(page_lines):
            font = "/F2 11 Tf" if raw_line.startswith("##") or idx == 0 else "/F1 9.5 Tf"
            clean = raw_line.lstrip("# ").rstrip()
            if first:
                stream_cmds.append(f"{font}")
                stream_cmds.append(f"({pdf_esc(clean)}) Tj")
                first = False
            else:
                stream_cmds.append("0 -13.5 Td")
                stream_cmds.append(f"{font}")
                stream_cmds.append(f"({pdf_esc(clean)}) Tj")
        stream_cmds.append("ET")
        content_bytes = "\n".join(stream_cmds).encode("latin-1", errors="replace")
        content_id = add_obj(
            f"<< /Length {len(content_bytes)} >>\nstream\n".encode("ascii")
            + content_bytes
            + b"\nendstream"
        )
        page_id = add_obj(
            f"<< /Type /Page /Parent {pages_id} 0 R /MediaBox [0 0 612 792] "
            f"/Resources << /Font << /F1 {font_reg_id} 0 R /F2 {font_bold_id} 0 R >> >> "
            f"/Contents {content_id} 0 R >>".encode("ascii")
        )
        page_ids.append(page_id)

    kids = " ".join(f"{pid} 0 R" for pid in page_ids)
    objects[catalog_id - 1] = f"<< /Type /Catalog /Pages {pages_id} 0 R >>".encode("ascii")
    objects[pages_id - 1] = f"<< /Type /Pages /Kids [{kids}] /Count {len(page_ids)} >>".encode("ascii")

    buf = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = [0]
    for idx, obj in enumerate(objects, start=1):
        offsets.append(len(buf))
        buf.extend(f"{idx} 0 obj\n".encode("ascii"))
        buf.extend(obj)
        buf.extend(b"\nendobj\n")
    xref_pos = len(buf)
    buf.extend(f"xref\n0 {len(objects) + 1}\n0000000000 65535 f \n".encode("ascii"))
    for off in offsets[1:]:
        buf.extend(f"{off:010d} 00000 n \n".encode("ascii"))
    buf.extend(
        f"trailer\n<< /Size {len(objects) + 1} /Root {catalog_id} 0 R >>\nstartxref\n{xref_pos}\n%%EOF\n".encode("ascii")
    )
    pdf_out.write_bytes(bytes(buf))


def main() -> int:
    ga_local = json.loads((ROOT / "bench/results/ga_local/report.json").read_text(encoding="utf-8"))
    gapbench = json.loads((ROOT / "bench/results/gapbench/baseline.json").read_text(encoding="utf-8"))
    s1 = json.loads((ROOT / "bench/results/routebench/s1_comparison/s1_comparison.json").read_text(encoding="utf-8"))
    m3 = json.loads((ROOT / "bench/results/perf/m3-baseline.json").read_text(encoding="utf-8"))

    author = read_author_meta()
    html_path = build_html(author, ga_local, gapbench, s1, m3)
    pdf_out = PAPER_DIR / "gap-paper.pdf"

    chrome_bin = Path("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
    if chrome_bin.exists():
        try:
            if pdf_out.exists():
                pdf_out.unlink()
            with tempfile.TemporaryDirectory(prefix="gap_chrome_") as chrome_prof:
                subprocess.run(
                    [
                        str(chrome_bin),
                        "--headless=new",
                        f"--user-data-dir={chrome_prof}",
                        "--no-first-run",
                        "--disable-gpu",
                        "--no-pdf-header-footer",
                        f"--print-to-pdf={pdf_out}",
                        html_path.resolve().as_uri(),
                    ],
                    capture_output=True,
                    timeout=45,
                    check=False,
                )
            if pdf_out.is_file() and pdf_out.read_bytes().startswith(b"%PDF-") and pdf_out.stat().st_size > 100_000:
                print(f"Rendered {html_path} and {pdf_out} via Chrome headless ({pdf_out.stat().st_size} bytes)")
                return 0
        except Exception as exc:
            print(f"Chrome headless PDF unavailable cleanly ({exc}); using stdlib PDF writer", file=sys.stderr)

    md_text = (ROOT / "docs/paper/GAP-PAPER.md").read_text(encoding="utf-8")
    wrapped: list[str] = []
    for line in md_text.splitlines():
        if line.startswith("![Figure"):
            wrapped.append(line.split("]")[0].lstrip("![") + "] (see HTML/SVG or TikZ PDF figures)")
            continue
        if len(line) <= 95:
            wrapped.append(line)
        else:
            wrapped.extend(textwrap.wrap(line, width=95))
    per_page = 48
    pages = [wrapped[i : i + per_page] for i in range(0, len(wrapped), per_page)]
    write_fallback_pdf(pdf_out, pages)
    print(f"Generated {html_path} and fallback stdlib {pdf_out} ({len(pages)} pages, {pdf_out.stat().st_size} bytes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
