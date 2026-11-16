#!/usr/bin/env python3
"""Render standalone printable paper/gap-paper.html and paper/gap-paper.pdf from verified paper sources."""

from __future__ import annotations

import html
import json
import shutil
import subprocess
import sys
import textwrap
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
PAPER_DIR = ROOT / "paper"


def build_html(
    author_line: str,
    ga_local: dict,
    gapbench: dict,
    s1: dict,
) -> Path:
    fig1 = (PAPER_DIR / "figures" / "fig1_architecture.svg").read_text(encoding="utf-8")
    fig2 = (PAPER_DIR / "figures" / "fig2_schema_scaling.svg").read_text(encoding="utf-8")
    fig3 = (PAPER_DIR / "figures" / "fig3_crossover_regimes.svg").read_text(encoding="utf-8")
    fig4 = (PAPER_DIR / "figures" / "fig4_runtime_latencies.svg").read_text(encoding="utf-8")

    # Build Table 1 rows directly from baseline.json
    t1_rows = []
    for c in gapbench["suite_comparisons"]:
        t1_rows.append(
            f"<tr><td>{html.escape(c['suite'])}</td><td>{c['n_tools']}</td>"
            f"<td>{c['direct']['uncached_tokens_per_success']:,.1f}</td>"
            f"<td>{c['gap']['uncached_tokens_per_success']:,.1f}</td>"
            f"<td>{c['uncached_token_ratio']:.3f}x</td>"
            f"<td>{c['direct']['cached_tokens_per_success']:,.1f}</td>"
            f"<td>{c['gap']['cached_tokens_per_success']:,.1f}</td>"
            f"<td>{c['cached_token_ratio']:.3f}x</td>"
            f"<td>{c['direct']['mean_round_trips']:.1f} / {c['gap']['mean_round_trips']:.1f}</td></tr>"
        )

    rt = ga_local["live_runtime_drills"]
    rep = ga_local["reproducible_build"]

    doc = f"""<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8"/>
<title>Graph Agent Protocol (GAP) — Academic Preprint</title>
<style>
  @page {{ size: letter; margin: 0.6in 0.55in; }}
  body {{
    font-family: "Times New Roman", Georgia, serif;
    color: #111827;
    max-width: 980px;
    margin: 24px auto;
    padding: 0 20px;
    line-height: 1.42;
    font-size: 10.5pt;
  }}
  header {{ text-align: center; margin-bottom: 18px; border-bottom: 1px solid #CBD5E1; padding-bottom: 12px; }}
  h1 {{ font-size: 18pt; color: #1B365D; margin: 0 0 8px 0; line-height: 1.2; }}
  .author {{ font-size: 10.5pt; color: #334155; margin: 4px 0; }}
  .abstract {{ background: #F8FAFC; border-left: 3px solid #1B365D; padding: 10px 14px; margin: 14px 0 18px 0; font-size: 9.8pt; }}
  .twocol {{ column-count: 2; column-gap: 22px; }}
  h2 {{ font-size: 12pt; color: #1B365D; border-bottom: 1px solid #E2E8F0; padding-bottom: 2px; break-after: avoid; }}
  h3 {{ font-size: 10.5pt; color: #0F172A; break-after: avoid; }}
  figure {{ column-span: all; margin: 14px 0; text-align: center; break-inside: avoid; }}
  figure svg {{ max-width: 100%; height: auto; border: 1px solid #E2E8F0; border-radius: 4px; }}
  figcaption {{ font-size: 9pt; color: #475569; margin-top: 4px; }}
  table {{ width: 100%; border-collapse: collapse; margin: 10px 0; font-size: 8.8pt; column-span: all; break-inside: avoid; }}
  th, td {{ border: 1px solid #CBD5E1; padding: 4px 6px; text-align: right; }}
  th:first-child, td:first-child {{ text-align: left; font-family: monospace; }}
  th {{ background: #F1F5F9; color: #1B365D; }}
  code {{ font-family: monospace; font-size: 9pt; background: #F1F5F9; padding: 0 3px; border-radius: 2px; }}
  @media print {{
    body {{ max-width: none; margin: 0; padding: 0; font-size: 9.5pt; }}
    .twocol {{ column-count: 2; column-gap: 18px; }}
  }}
</style>
</head>
<body>
<header>
  <h1>Graph Agent Protocol (GAP): Unifying Typed Discovery, Batched Federation, and Zero-Trust Governance for AI Agents</h1>
  <div class="author">{html.escape(author_line)}</div>
  <div class="author">Reference Release <code>v0.1.0-rc.1</code> &middot; November 16, 2026 &middot; Apache-2.0</div>
</header>

<div class="abstract">
  <strong>Abstract.</strong> We introduce <strong>Graph Agent Protocol (GAP)</strong>, a strongly typed GraphQL-profile gateway unifying APIs, SQL stores, gRPC/GraphQL services, codebases, MCP servers, and A2A tasks across three separated architectural planes: (1) a <em>Zero-ML deterministic execution plane</em> with Cedar authorization, tenant-scoped planning, batched DataLoader joins, and wait-free immutable snapshot pinning; (2) a <em>three-level progressive discovery plane</em> holding static MCP <code>tools/list</code> cost invariant at <strong>651 tokens</strong> across N=10..1,000 tools; and (3) a <em>zero-trust control plane</em> preventing MCP rug pulls via description-hash drift freezes, DNS-pinned egress SSRF defense, OS sandboxing, and BLAKE3 hash-chained audit logs. Implemented across 31 Rust crates and verified across 2,481 spec-trace markers, GAP reduces cross-branch token cost at N=1,000 by <strong>99.0%</strong> uncached (<strong>93.5%</strong> cached) and code-graph context by <strong>119.3x-208.7x</strong>, cold-starts in <strong>{rt['cold_start_ms']:.1f} ms</strong> at <strong>{rt['idle_rss_mib']:.2f} MiB</strong> idle RSS, sustains <strong>{rt['measured_rps']:.1f} req/s</strong> with <strong>{rt['errors']}</strong> errors at <strong>{rt['p95_ms']:.2f} ms</strong> p95, and recovers from OS SIGKILL in <strong>{rt['sigkill_recovery_ready_ms']:.2f} ms</strong>.
</div>

<figure>
  {fig1}
  <figcaption><strong>Figure 1:</strong> Three-plane GAP architecture separating deterministic hot execution, calibrated progressive discovery, and zero-trust snapshot control.</figcaption>
</figure>

<div class="twocol">
  <h2>1. Introduction &amp; Threat Model</h2>
  <p>AI agent stacks split tool invocation (Anthropic MCP) from horizontal agent tasks (Google A2A). Publishing flat MCP JSON Schemas inflates prompts linearly from 1,904 tokens at N=10 to <strong>203,421 tokens</strong> at N=1,000, induces N+1 client join loops, and exposes hosts to descriptor rug pulls documented in arXiv security audits (arXiv:2506.02040, arXiv:2508.14925, arXiv:2512.06556). GAP enforces 20 normative invariants (<code>INV-01</code>..<code>INV-20</code>) and zero ML on the typed execution hot path (<code>ARC-002</code>).</p>

  <h2>2. Protocol &amp; Security Controls</h2>
  <p>GAP-QL is a strict GraphQL profile parsed by <code>apollo-compiler</code> behind a constant 5-tool MCP <code>2026-07-28</code> facade (<code>gap_search</code>, <code>gap_describe</code>, <code>gap_query</code>, <code>gap_mutate</code>, <code>gap_task</code>) and A2A v1.0 task APIs. All branches carry explicit trust tiers (<code>T0</code> Ed25519 signed, <code>T1</code> admin-approved, <code>T2</code> untrusted wrapped in escaped <code>&lt;gap:untrusted&gt;</code> tags). Mutations require Cedar verification and verified <code>@sideEffect</code> classifications. Background <code>DriftMonitor</code> freezes branches immediately to <code>BranchStatus::Drift</code> upon unapproved description changes (<code>SEC-082</code>).</p>

  <h2>3. Reference Implementation &amp; Verification</h2>
  <p>Built in Rust Edition 2024 (<code>rustc 1.98.1</code>) across 31 crates (<code>L0</code>-<code>L6</code>), excluding BSL-1.1 dependencies from default builds (CycloneDX 1.5 SBOM of 398 components). Offline verification passes across 354 spec requirements (82 MUST, 0 uncovered across 2,481 markers), 135/136 conformance checks (133 batch + 2 loopback HTTP socket kits), 365 1,024-case proptests, 10 fuzz seed corpora, and identical double-build SHA-256 digests (<code>{rep['build_1_sha256'][:16]}...</code>, {rep['binary_bytes']:,} bytes).</p>
</div>

<figure>
  {fig2}
  <figcaption><strong>Figure 2:</strong> Constant O(1) 651-token MCP schema overhead vs O(N) Direct-MCP growth up to 203,421 tokens at N=1,000.</figcaption>
</figure>

<table>
  <thead>
    <tr><th>Suite</th><th>N</th><th>Direct Unc.</th><th>GAP Unc.</th><th>Ratio (U)</th><th>Direct Cac.</th><th>GAP Cac.</th><th>Ratio (C)</th><th>Turns D/G</th></tr>
  </thead>
  <tbody>
    {''.join(t1_rows)}
  </tbody>
</table>

<figure>
  {fig3}
  <figcaption><strong>Figure 3:</strong> Token ratio GAP / Direct-MCP (log scale) capturing both 95x-209x wins and negative N&lt;=50 cached crossover cells.</figcaption>
</figure>

<figure>
  {fig4}
  <figcaption><strong>Figure 4:</strong> Multi-scale latency spectrum from nanosecond lock-free RCU primitives to live mixed HTTP load and OS SIGKILL recovery.</figcaption>
</figure>

<div class="twocol">
  <h2>4. RouteBench &amp; Live Runtime Evaluation</h2>
  <p>On RouteBench (n=48 test + 72 held-out), un-finetuned Candle weights (<code>laya-candle:f32</code>) achieve <code>{s1['s1_debiased']['test_metrics']['top1']['estimate']*100:.2f}%</code> debiased top-1 at <code>{s1['s1_debiased']['test_metrics']['p95_latency_ms']:.2f} ms</code> p95, whereas GAP's deterministic lexical+structural baseline reaches <strong>{s1['baseline']['test_metrics']['top1']['estimate']*100:.2f}%</strong> test / <strong>{s1['baseline']['held_out_metrics']['top1']['estimate']*100:.2f}%</strong> held-out top-1 in <strong>{s1['baseline']['test_metrics']['p95_latency_ms']:.3f} ms</strong>—justifying fail-safe <code>BASELINE_FALLBACK</code> deployment (<code>INV-17</code>).</p>
  <p>In live release-binary drills (<code>bench/results/ga_local/report.json</code>), GAP boots to HTTP 200 <code>/readyz</code> in <strong>{rt['cold_start_ms']:.2f} ms</strong>, occupies <strong>{rt['idle_rss_mib']:.2f} MiB</strong> idle RSS ({rt['post_load_rss_mib']:.2f} MiB post-load), processes {rt['mixed_load_requests']} mixed requests across {rt['concurrency']} threads at <strong>{rt['measured_rps']:.1f} req/s</strong> ({rt['p50_ms']:.3f} ms p50 / {rt['p95_ms']:.3f} ms p95 / {rt['p99_ms']:.3f} ms p99, {rt['errors']} errors), and restores verified traffic after <code>SIGKILL</code> in <strong>{rt['sigkill_recovery_ready_ms']:.2f} ms</strong>.</p>
</div>
</body>
</html>
"""
    html_out = PAPER_DIR / "gap-paper.html"
    html_out.write_text(doc, encoding="utf-8")
    return html_out


def write_fallback_pdf(pdf_out: Path, lines_by_page: list[list[str]]) -> None:
    """Emit a clean, standards-compliant multi-page PDF 1.4 using built-in Helvetica fonts."""
    def pdf_esc(s: str) -> str:
        return s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")

    objects: list[bytes] = []

    def add_obj(body: bytes) -> int:
        objects.append(body)
        return len(objects)

    catalog_id = add_obj(b"")  # placeholder 1
    pages_id = add_obj(b"")    # placeholder 2
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

    # Read author name cleanly from author_config.tex
    author_tex = (PAPER_DIR / "author_config.tex").read_text(encoding="utf-8")
    author_name = "Mike (GAP Project Lead)"
    for line in author_tex.splitlines():
        if "\\GapAuthorFullName" in line:
            author_name = line.split("{")[-1].rstrip("}")
            break

    html_path = build_html(author_name, ga_local, gapbench, s1)
    pdf_out = PAPER_DIR / "gap-paper.pdf"

    # Attempt sandbox-safe headless Chrome rendering first (NO --no-sandbox flag).
    chrome_bin = Path("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
    if chrome_bin.exists():
        try:
            subprocess.run(
                [
                    str(chrome_bin),
                    "--headless",
                    "--disable-gpu",
                    "--no-pdf-header-footer",
                    f"--print-to-pdf={pdf_out}",
                    html_path.resolve().as_uri(),
                ],
                capture_output=True,
                timeout=20,
                check=True,
            )
            if pdf_out.is_file() and pdf_out.read_bytes().startswith(b"%PDF-") and pdf_out.stat().st_size > 10_000:
                print(f"Rendered {html_path} and {pdf_out} via Chrome headless ({pdf_out.stat().st_size} bytes)")
                return 0
        except Exception as exc:
            print(f"Chrome headless PDF unavailable cleanly ({exc}); using stdlib PDF writer", file=sys.stderr)

    # Clean deterministic stdlib fallback derived directly from docs/paper/GAP-PAPER.md
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
