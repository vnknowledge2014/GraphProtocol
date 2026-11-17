#!/usr/bin/env python3
"""Generate publication-quality SVG and pure-TikZ (.tex) figures directly from measured JSON artifacts."""

from __future__ import annotations

import json
import math
import xml.sax.saxutils as sax
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
FIG_DIR = ROOT / "paper" / "figures"

# Colorblind-safe Okabe-Ito inspired palette
NAVY = "#1B365D"
BLUE = "#0072B2"
ORANGE = "#D55E00"
TEAL = "#009E73"
PURPLE = "#7A5195"
SLATE = "#475569"
LIGHT_BG = "#F8FAFC"
BORDER = "#CBD5E1"


def esc_xml(s: str) -> str:
    return sax.escape(s)


def esc_tex(s: str) -> str:
    return (
        s.replace("\\", r"\textbackslash{}")
        .replace("&", r"\&")
        .replace("%", r"\%")
        .replace("$", r"\$")
        .replace("#", r"\#")
        .replace("_", r"\_")
    )


def generate_fig1_architecture() -> tuple[str, str]:
    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 940 500" width="940" height="500" font-family="Helvetica, Arial, sans-serif">
  <defs>
    <marker id="arr-navy" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
      <path d="M 0 1 L 10 5 L 0 9 z" fill="{NAVY}"/>
    </marker>
    <marker id="arr-teal" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
      <path d="M 0 1 L 10 5 L 0 9 z" fill="{TEAL}"/>
    </marker>
    <marker id="arr-orange" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
      <path d="M 0 1 L 10 5 L 0 9 z" fill="{ORANGE}"/>
    </marker>
  </defs>
  <rect x="0" y="0" width="940" height="500" fill="#FFFFFF"/>

  <!-- Agent Hosts & Dual-Stack Edge -->
  <rect x="24" y="18" width="892" height="72" rx="8" fill="{LIGHT_BG}" stroke="{NAVY}" stroke-width="1.5"/>
  <text x="42" y="42" font-size="13" font-weight="700" fill="{NAVY}">AI Agent Hosts &amp; Dual-Stack Transport Ingress (L4)</text>
  <text x="42" y="61" font-size="10.5" fill="{SLATE}">MCP 2026-07-28 (651 tok)  |  GAP-QL (/graphql + SSE)  |  A2A v1.0 Tasks  |  AG-UI v1.0 (/ag-ui SSE &amp; Interrupts)  |  CLI</text>
  <text x="42" y="79" font-size="10.5" font-weight="600" fill="{BLUE}">Wire Edge: HTTP/1.1 + HTTP/2 TCP (Alt-Svc)  ·  HTTP/3 QUIC UDP (RFC 9114, TLS 1.3 ALPN h3, 0-RTT disabled)  ·  gRPC  ·  stdio</text>

  <!-- Plane 1: Deterministic Execution -->
  <rect x="24" y="122" width="556" height="222" rx="8" fill="#EFF6FF" stroke="{BLUE}" stroke-width="1.8"/>
  <text x="42" y="145" font-size="12.5" font-weight="700" fill="{BLUE}">Plane 1: Deterministic Hot Execution Plane (Zero ML · INV-01 / INV-02 / INV-04)</text>

  <rect x="40" y="160" width="122" height="68" rx="6" fill="#FFFFFF" stroke="{BLUE}" stroke-width="1.2"/>
  <text x="101" y="182" text-anchor="middle" font-size="11" font-weight="700" fill="{NAVY}">AuthN + Cedar</text>
  <text x="101" y="199" text-anchor="middle" font-size="9.8" fill="{SLATE}">Default-Deny Policy</text>
  <text x="101" y="214" text-anchor="middle" font-size="9.8" fill="{SLATE}">Scopes &amp; Cost Caps</text>

  <rect x="174" y="160" width="124" height="68" rx="6" fill="#FFFFFF" stroke="{BLUE}" stroke-width="1.2"/>
  <text x="236" y="182" text-anchor="middle" font-size="11" font-weight="700" fill="{NAVY}">GAP-QL Planner</text>
  <text x="236" y="199" text-anchor="middle" font-size="9.8" fill="{SLATE}">ArcSwap + AVL OrdMap</text>
  <text x="236" y="214" text-anchor="middle" font-size="9.8" fill="{SLATE}">Tenant PlanCache</text>

  <rect x="310" y="160" width="124" height="68" rx="6" fill="#FFFFFF" stroke="{BLUE}" stroke-width="1.2"/>
  <text x="372" y="182" text-anchor="middle" font-size="11" font-weight="700" fill="{NAVY}">Batch Executor</text>
  <text x="372" y="199" text-anchor="middle" font-size="9.8" fill="{SLATE}">DataLoader Join O(1)</text>
  <text x="372" y="214" text-anchor="middle" font-size="9.8" fill="{SLATE}">P2C LB + Breaker</text>

  <rect x="446" y="160" width="118" height="68" rx="6" fill="#FFFFFF" stroke="{BLUE}" stroke-width="1.2"/>
  <text x="505" y="182" text-anchor="middle" font-size="11" font-weight="700" fill="{NAVY}">Security Guard</text>
  <text x="505" y="199" text-anchor="middle" font-size="9.8" fill="{SLATE}">EgressGuard SSRF</text>
  <text x="505" y="214" text-anchor="middle" font-size="9.8" fill="{SLATE}">OS Sandbox &amp; T2 Tag</text>

  <!-- Sub-box inside Plane 1 -->
  <rect x="40" y="244" width="524" height="84" rx="6" fill="#FFFFFF" stroke="{BORDER}" stroke-width="1.2"/>
  <text x="302" y="267" text-anchor="middle" font-size="11.5" font-weight="700" fill="{NAVY}">Native Graph Connector (NGC) Adapters + Durable Multi-Tenant State</text>
  <text x="302" y="287" text-anchor="middle" font-size="10.2" fill="{SLATE}">OpenAPI  |  GraphQL  |  gRPC  |  SQL (SQLite/PostgreSQL)  |  Sandboxed CLI  |  Code Graph (tree-sitter)</text>
  <text x="302" y="306" text-anchor="middle" font-size="10.2" fill="{TEAL}">ContextStore (SQLite WAL/FTS5 + opt. SurrealDB)  ·  TaskStore Leases  ·  BLAKE3 Audit Chain</text>

  <!-- Plane 2: Discovery -->
  <rect x="604" y="122" width="312" height="104" rx="8" fill="#ECFDF5" stroke="{TEAL}" stroke-width="1.8"/>
  <text x="620" y="145" font-size="12.5" font-weight="700" fill="{TEAL}">Plane 2: Calibrated Discovery</text>
  <text x="620" y="166" font-size="10.5" font-weight="600" fill="{NAVY}">__metaGraph (L0) -&gt; __search (L1) -&gt; __type (L2)</text>
  <text x="620" y="185" font-size="10" fill="{SLATE}">BM25 + HNSW Hybrid + Visibility Bitset (INV-05)</text>
  <text x="620" y="203" font-size="10" fill="{SLATE}">Opt. S1 (Laya, Jev, Gemini Dec. 1) + Fallback (INV-17)</text>

  <!-- Plane 3: Ingestion & Snapshot Control -->
  <rect x="604" y="240" width="312" height="104" rx="8" fill="#FFF7ED" stroke="{ORANGE}" stroke-width="1.8"/>
  <text x="620" y="263" font-size="12.5" font-weight="700" fill="{ORANGE}">Plane 3: Zero-Trust Control Plane</text>
  <text x="620" y="284" font-size="10.5" font-weight="600" fill="{NAVY}">Propose-Not-Activate + T0 / T1 / T2 Trust Tiers</text>
  <text x="620" y="303" font-size="10" fill="{SLATE}">SHA-256 Desc-Hash Pin -&gt; Drift Freeze (SEC-082)</text>
  <text x="620" y="321" font-size="10" fill="{SLATE}">Wait-Free ArcSwap Publish + Auto-Rollback</text>

  <!-- Bottom Cross-cutting Invariant strip -->
  <rect x="24" y="366" width="892" height="114" rx="8" fill="{LIGHT_BG}" stroke="{BORDER}" stroke-width="1.4"/>
  <text x="42" y="390" font-size="12" font-weight="700" fill="{NAVY}">Core Architectural Guarantees &amp; Verified Runtime Invariants</text>
  <text x="42" y="411" font-size="10.5" fill="{SLATE}">1. Wait-Free Snapshot Atomicity (INV-01, INV-02): Each request pins one Arc&lt;Snapshot&gt; backed by pure-Rust persistent AVL OrdMap.</text>
  <text x="42" y="430" font-size="10.5" fill="{SLATE}">2. Multi-Tenant &amp; Supply-Chain Hygiene (INV-09, ADR-002): TenantId in all keys; zero unsafe outside S1; zero RUSTSEC advisories.</text>
  <text x="42" y="449" font-size="10.5" fill="{SLATE}">3. Rug-Pull &amp; Replay Defense (INV-08, OPS-005): Drift freezes mutations; T2 output in &lt;gap:untrusted&gt;; HTTP/3 0-RTT disabled.</text>
  <text x="42" y="468" font-size="10.5" fill="{SLATE}">4. Strict Layering (ARC-002): L0..L6 acyclic crate DAG ensures zero ML runtime crates reach execution, transport, or authz.</text>

  <!-- Directional Connectors -->
  <line x1="290" y1="90" x2="290" y2="119" stroke="{NAVY}" stroke-width="1.8" marker-end="url(#arr-navy)"/>
  <line x1="760" y1="90" x2="760" y2="119" stroke="{TEAL}" stroke-width="1.8" marker-end="url(#arr-teal)"/>
  <line x1="604" y1="292" x2="583" y2="292" stroke="{ORANGE}" stroke-width="1.8" stroke-dasharray="5 3" marker-end="url(#arr-orange)"/>
</svg>"""

    tex = r"""\begin{tikzpicture}[
  box/.style={draw, rounded corners=3pt, align=center, font=\scriptsize, inner sep=4pt, fill=white},
  plane/.style={draw, rounded corners=4pt, thick, inner sep=6pt},
  >=stealth
]
  \node[box, fill=gray!8, minimum width=15.6cm, draw=black!70] (hosts) at (0, 3.65)
    {\textbf{AI Agent Hosts \& Dual-Stack Transport Ingress (\texttt{L4})}\\
     MCP \texttt{2026-07-28} (651 tok) $\mid$ GAP-QL (\texttt{/graphql} + SSE) $\mid$ A2A v1.0 $\mid$ AG-UI v1.0 (\texttt{/ag-ui} SSE) $\mid$ CLI\\
     \scriptsize HTTP/1.1 + HTTP/2 TCP (\texttt{Alt-Svc}) $\cdot$ HTTP/3 QUIC UDP (\texttt{RFC 9114}, TLS 1.3, 0-RTT off) $\cdot$ gRPC $\cdot$ \texttt{stdio}};

  \draw[fill=blue!4, draw=blue!70!black, rounded corners=4pt, thick] (-7.8, -0.35) rectangle (1.9, 2.75);
  \node[anchor=west, font=\scriptsize\bfseries, text=blue!60!black] at (-7.65, 2.50)
    {Plane 1: Deterministic Execution (Zero ML on Hot Path, ARC-002 / INV-01 / INV-02)};

  \node[box, draw=blue!60!black, minimum width=2.15cm, minimum height=0.95cm] (auth) at (-6.55, 1.65)
    {\textbf{AuthN + Cedar}\\Default-Deny\\INV-04};
  \node[box, draw=blue!60!black, minimum width=2.15cm, minimum height=0.95cm] (plan) at (-4.15, 1.65)
    {\textbf{GAP-QL Plan}\\ArcSwap + OrdMap\\Tenant Cache};
  \node[box, draw=blue!60!black, minimum width=2.15cm, minimum height=0.95cm] (exec) at (-1.75, 1.65)
    {\textbf{Batch Exec}\\DataLoader\\P2C + Breaker};
  \node[box, draw=blue!60!black, minimum width=2.15cm, minimum height=0.95cm] (guard) at (0.65, 1.65)
    {\textbf{Sec Guard}\\EgressGuard\\Sandbox + T2};

  \node[box, draw=black!40, minimum width=9.2cm, minimum height=0.82cm] (ngc) at (-2.95, 0.32)
    {\textbf{NGC Adapters \& Durable Stores}\\
     OpenAPI $\cdot$ GraphQL $\cdot$ gRPC $\cdot$ SQL $\cdot$ CLI $\cdot$ Code Graph (\texttt{tree-sitter}) $\cdot$ SQLite Context/Tasks/BLAKE3 Audit};

  \draw[fill=teal!6, draw=teal!70!black, rounded corners=4pt, thick] (2.25, 1.35) rectangle (7.8, 2.75);
  \node[anchor=west, font=\scriptsize\bfseries, text=teal!60!black] at (2.4, 2.50) {Plane 2: Calibrated Discovery};
  \node[anchor=west, font=\tiny, align=left] at (2.4, 1.90)
    {\texttt{\_\_metaGraph} ($L_0$) $\rightarrow$ \texttt{\_\_search} ($L_1$) $\rightarrow$ \texttt{\_\_type} ($L_2$)\\
     BM25 + HNSW + Visibility Bitset (\texttt{INV-05})\\
     S1 (\texttt{Laya}/\texttt{Jev}/\texttt{Gemini Dec.1}) + Fallback (\texttt{INV-17})};

  \draw[fill=orange!6, draw=orange!80!black, rounded corners=4pt, thick] (2.25, -0.35) rectangle (7.8, 1.12);
  \node[anchor=west, font=\scriptsize\bfseries, text=orange!70!black] at (2.4, 0.87) {Plane 3: Zero-Trust Control};
  \node[anchor=west, font=\tiny, align=left] at (2.4, 0.25)
    {Propose-Not-Activate + $T_0/T_1/T_2$ Trust Tiers\\
     Desc-Hash Pin $\rightarrow$ Drift Freeze (\texttt{SEC-082})\\
     Wait-Free Swap + Error-Spike Rollback};

  \node[box, fill=gray!6, draw=black!45, minimum width=15.6cm, align=left] (inv) at (0, -1.25)
    {\textbf{Cross-Cutting Invariants:} \texttt{INV-01} single snapshot per request; \texttt{INV-09} \texttt{TenantId} in every key; \texttt{INV-15} \texttt{Secret<T>} redaction; \texttt{OPS-005} 0-RTT disabled; \texttt{SEC-080} \texttt{<gap:untrusted>} tagging.};

  \draw[->, thick] (-2.95, 3.00) -- (-2.95, 2.76);
  \draw[->, thick, draw=teal!70!black] (5.0, 3.00) -- (5.0, 2.76);
  \draw[->, dashed, thick, draw=orange!80!black] (2.25, 0.38) -- (1.90, 0.38);
\end{tikzpicture}
"""
    return svg, tex


def generate_fig2_schema_scaling(gapbench: dict) -> tuple[str, str]:
    rows = gapbench["discovery_scaling"]
    w, h = 820, 400
    pad_l, pad_r, pad_t, pad_b = 82, 168, 66, 60
    pw = w - pad_l - pad_r
    ph = h - pad_t - pad_b

    y_min_log, y_max_log = 1.5, 5.95
    xs = [pad_l + pw * (i + 0.5) / len(rows) for i in range(len(rows))]

    def y_px(val: float) -> float:
        lv = math.log10(max(val, 1.0))
        frac = (lv - y_min_log) / (y_max_log - y_min_log)
        return pad_t + ph * (1.0 - frac)

    bars = []
    for i, row in enumerate(rows):
        cx = xs[i]
        bw = 38
        d_val = row["direct_tools_list_tokens"]
        d_y = y_px(d_val)
        d_h = (pad_t + ph) - d_y
        bars.append(f'<rect x="{cx - bw - 5:.1f}" y="{d_y:.1f}" width="{bw}" height="{d_h:.1f}" fill="{ORANGE}" rx="3"/>')
        bars.append(f'<text x="{cx - bw/2 - 5:.1f}" y="{d_y - 6:.1f}" text-anchor="middle" font-size="10.5" font-weight="700" fill="{ORANGE}">{d_val:,}</text>')

        g_val = row["gap_tools_list_tokens"]
        g_y = y_px(g_val)
        g_h = (pad_t + ph) - g_y
        bars.append(f'<rect x="{cx + 5:.1f}" y="{g_y:.1f}" width="{bw}" height="{g_h:.1f}" fill="{BLUE}" rx="3"/>')
        bars.append(f'<text x="{cx + bw/2 + 5:.1f}" y="{g_y - 6:.1f}" text-anchor="middle" font-size="10.5" font-weight="700" fill="{BLUE}">{g_val:,}</text>')

        bars.append(f'<text x="{cx:.1f}" y="{pad_t + ph + 20}" text-anchor="middle" font-size="11.5" font-weight="700" fill="{NAVY}">N = {row["n_tools"]}</text>')
        bars.append(f'<text x="{cx:.1f}" y="{pad_t + ph + 36}" text-anchor="middle" font-size="10" fill="{SLATE}">({row["branches"]} branches)</text>')

    grid = []
    for exp in (2, 3, 4, 5):
        val = 10**exp
        gy = y_px(val)
        grid.append(f'<line x1="{pad_l}" y1="{gy:.1f}" x2="{pad_l + pw}" y2="{gy:.1f}" stroke="{BORDER}" stroke-dasharray="3 3"/>')
        grid.append(f'<text x="{pad_l - 8}" y="{gy + 4:.1f}" text-anchor="end" font-size="10.5" fill="{SLATE}">{val:,}</text>')

    gate_y = y_px(1200)
    # Right annotation gutter guarantees ZERO collision with N=1000 orange bar
    gutter_x = pad_l + pw + 8
    grid.append(f'<line x1="{pad_l}" y1="{gate_y:.1f}" x2="{pad_l + pw}" y2="{gate_y:.1f}" stroke="{TEAL}" stroke-width="1.8" stroke-dasharray="6 4"/>')
    grid.append(f'<line x1="{pad_l + pw}" y1="{gate_y:.1f}" x2="{gutter_x + 4}" y2="{gate_y:.1f}" stroke="{TEAL}" stroke-width="1.5"/>')
    grid.append(f'<rect x="{gutter_x + 4}" y="{gate_y - 18:.1f}" width="146" height="36" rx="4" fill="#ECFDF5" stroke="{TEAL}" stroke-width="1.2"/>')
    grid.append(f'<text x="{gutter_x + 12}" y="{gate_y - 3:.1f}" font-size="10.5" font-weight="700" fill="{TEAL}">Gate G-F1 Ceiling</text>')
    grid.append(f'<text x="{gutter_x + 12}" y="{gate_y + 11:.1f}" font-size="10" fill="{NAVY}">&lt;= 1,200 tokens (O(1))</text>')

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" font-family="Helvetica, Arial, sans-serif">
  <rect x="0" y="0" width="{w}" height="{h}" fill="#FFFFFF"/>
  <!-- Dedicated Legend Header Band (completely above plot axes) -->
  <rect x="{pad_l}" y="14" width="{pw}" height="32" rx="5" fill="{LIGHT_BG}" stroke="{BORDER}"/>
  <rect x="{pad_l + 14}" y="24" width="12" height="12" fill="{ORANGE}" rx="2"/>
  <text x="{pad_l + 32}" y="34" font-size="11" font-weight="600" fill="{NAVY}">Direct-MCP Schema (O(N): 1,904 -&gt; 203,421)</text>
  <rect x="{pad_l + 305}" y="24" width="12" height="12" fill="{BLUE}" rx="2"/>
  <text x="{pad_l + 323}" y="34" font-size="11" font-weight="600" fill="{NAVY}">GAP 5-Tool Facade (constant O(1): 651)</text>

  {''.join(grid)}
  <line x1="{pad_l}" y1="{pad_t}" x2="{pad_l}" y2="{pad_t + ph}" stroke="{NAVY}" stroke-width="1.5"/>
  <line x1="{pad_l}" y1="{pad_t + ph}" x2="{pad_l + pw}" y2="{pad_t + ph}" stroke="{NAVY}" stroke-width="1.5"/>
  {''.join(bars)}
  <text x="20" y="{pad_t + ph/2}" text-anchor="middle" font-size="11.5" font-weight="700" fill="{NAVY}" transform="rotate(-90 20 {pad_t + ph/2})">MCP tools/list Tokens (log10)</text>
</svg>"""

    tex_bars = []
    for i, row in enumerate(rows):
        x0 = i * 1.9
        d_val = row["direct_tools_list_tokens"]
        g_val = row["gap_tools_list_tokens"]
        dy = (math.log10(d_val) - 1.5) * 0.85
        gy = (math.log10(g_val) - 1.5) * 0.85
        tex_bars.append(
            f"  \\fill[orange!80!black] ({x0:.2f},0) rectangle ({x0 + 0.55:.2f},{dy:.3f});\n"
            f"  \\node[above, font=\\tiny, text=orange!70!black] at ({x0 + 0.275:.2f},{dy:.3f}) {{{d_val:,}}};\n"
            f"  \\fill[blue!75!black] ({x0 + 0.65:.2f},0) rectangle ({x0 + 1.20:.2f},{gy:.3f});\n"
            f"  \\node[above, font=\\tiny, text=blue!70!black] at ({x0 + 0.925:.2f},{gy:.3f}) {{{g_val:,}}};\n"
            f"  \\node[below, font=\\scriptsize] at ({x0 + 0.60:.2f},0) {{$N={row['n_tools']}$}};"
        )
    gate_tex_y = (math.log10(1200.0) - 1.5) * 0.85
    tex = "\n".join([
        r"\begin{tikzpicture}[x=0.95cm, y=0.95cm]",
        r"  \draw[->] (-0.3,0) -- (7.5,0);",
        r"  \draw[->] (-0.3,0) -- (-0.3,3.85) node[above, font=\scriptsize] {\texttt{tools/list} Tokens ($\log_{10}$)};",
        f"  \\draw[dashed, teal!75!black, thick] (-0.3,{gate_tex_y:.3f}) -- (7.6,{gate_tex_y:.3f}) node[right, font=\\tiny, text=teal!75!black] {{Gate G-F1 (1,200)}};",
        *[
            f"  \\draw[gray!35, dotted] (-0.3,{(exp - 1.5) * 0.85:.3f}) node[left, font=\\tiny, text=black] {{$10^{exp}$}} -- (7.3,{(exp - 1.5) * 0.85:.3f});"
            for exp in (2, 3, 4, 5)
        ],
        *tex_bars,
        r"  \fill[orange!80!black] (0.0,3.55) rectangle (0.25,3.72) node[right, xshift=4pt, yshift=-2pt, font=\tiny, text=black] {Direct-MCP ($O(N)$)};",
        r"  \fill[blue!75!black] (2.9,3.55) rectangle (3.15,3.72) node[right, xshift=4pt, yshift=-2pt, font=\tiny, text=black] {GAP Facade ($O(1) = 651$)};",
        r"\end{tikzpicture}",
    ])
    return svg, tex


def _fmt_ratio(r: float) -> str:
    if r >= 1.0:
        return f"{r:.2f}x"
    if r >= 0.05:
        return f"{r:.3f}x"
    return f"{r:.4f}x"


def generate_fig3_crossover_regimes(gapbench: dict) -> tuple[str, str]:
    comps = gapbench["suite_comparisons"]
    labels = []
    uncached = []
    cached = []
    for c in comps:
        suite = c["suite"]
        n = c["n_tools"]
        short = "API" if suite == "api_lookup" else ("Join" if suite == "cross_branch_join" else "Code")
        labels.append(f"{short}\nN={n}")
        uncached.append(c["uncached_token_ratio"])
        cached.append(c["cached_token_ratio"])

    w, h = 880, 440
    pad_l, pad_r, pad_t, pad_b = 84, 34, 68, 72
    pw = w - pad_l - pad_r
    ph = h - pad_t - pad_b
    y_min_log, y_max_log = -2.85, 1.42  # ample headroom above 8.58x and below 0.0048x

    def y_px(val: float) -> float:
        lv = math.log10(max(val, 0.0014))
        frac = (lv - y_min_log) / (y_max_log - y_min_log)
        return pad_t + ph * (1.0 - frac)

    y_parity = y_px(1.0)
    grid = [
        f'<rect x="{pad_l}" y="{pad_t}" width="{pw}" height="{y_parity - pad_t:.1f}" fill="#FEF2F2" opacity="0.7"/>',
        f'<rect x="{pad_l}" y="{y_parity:.1f}" width="{pw}" height="{pad_t + ph - y_parity:.1f}" fill="#ECFDF5" opacity="0.6"/>',
    ]
    for val, lbl in ((0.01, "0.01x"), (0.1, "0.1x"), (1.0, "1.0x (Parity)"), (10.0, "10.0x")):
        gy = y_px(val)
        col = NAVY if val == 1.0 else BORDER
        sw = "1.8" if val == 1.0 else "1"
        dash = "6 4" if val == 1.0 else "3 3"
        grid.append(f'<line x1="{pad_l}" y1="{gy:.1f}" x2="{pad_l + pw}" y2="{gy:.1f}" stroke="{col}" stroke-width="{sw}" stroke-dasharray="{dash}"/>')
        grid.append(f'<text x="{pad_l - 8}" y="{gy + 4:.1f}" text-anchor="end" font-size="10.5" fill="{NAVY if val == 1.0 else SLATE}">{lbl}</text>')

    items = []
    step = pw / len(labels)
    for i, lbl in enumerate(labels):
        cx = pad_l + step * (i + 0.5)
        bw = 23
        u_r = uncached[i]
        c_r = cached[i]
        # Pair of bars separated by 6 px gap so labels have clean spacing
        specs = ((u_r, BLUE, -bw - 3, 0), (c_r, ORANGE, 3, 1))
        for ratio, col, offset, series_idx in specs:
            yp = y_px(ratio)
            top = min(yp, y_parity)
            ht = max(2.0, abs(yp - y_parity))
            items.append(f'<rect x="{cx + offset:.1f}" y="{top:.1f}" width="{bw}" height="{ht:.1f}" fill="{col}" rx="2"/>')
            # Stagger label y by 11px when two same-sign tight bars could sit at almost identical height (Code N=24)
            if ratio >= 1.0:
                txt_y = yp - (6 if series_idx == 0 else 6)
            else:
                extra = 11 if (i == len(labels) - 1 and series_idx == 1) else 0
                txt_y = yp + 12 + extra
            lbl_x = cx + offset + bw / 2 + (-3 if series_idx == 0 else 3)
            items.append(f'<text x="{lbl_x:.1f}" y="{txt_y:.1f}" text-anchor="middle" font-size="8.2" font-weight="700" fill="{col}">{_fmt_ratio(ratio)}</text>')
        l1, l2 = lbl.split("\n")
        items.append(f'<text x="{cx:.1f}" y="{pad_t + ph + 18}" text-anchor="middle" font-size="10.5" font-weight="700" fill="{NAVY}">{l1}</text>')
        items.append(f'<text x="{cx:.1f}" y="{pad_t + ph + 33}" text-anchor="middle" font-size="10" fill="{SLATE}">{l2}</text>')

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" font-family="Helvetica, Arial, sans-serif">
  <rect x="0" y="0" width="{w}" height="{h}" fill="#FFFFFF"/>
  <!-- Top Legend Band completely outside data region -->
  <rect x="{pad_l}" y="14" width="{pw}" height="32" rx="5" fill="{LIGHT_BG}" stroke="{BORDER}"/>
  <rect x="{pad_l + 14}" y="24" width="12" height="12" fill="{BLUE}" rx="2"/>
  <text x="{pad_l + 32}" y="34" font-size="11" font-weight="600" fill="{NAVY}">Uncached Token Ratio (GAP / Direct)</text>
  <rect x="{pad_l + 275}" y="24" width="12" height="12" fill="{ORANGE}" rx="2"/>
  <text x="{pad_l + 293}" y="34" font-size="11" font-weight="600" fill="{NAVY}">Cached Token Ratio (0.10x Warm Prefix)</text>
  <text x="{pad_l + pw - 12}" y="34" text-anchor="end" font-size="10.5" font-weight="700" fill="{TEAL}">&lt; 1.0x = GAP Saves Tokens</text>

  {''.join(grid)}
  <line x1="{pad_l}" y1="{pad_t}" x2="{pad_l}" y2="{pad_t + ph}" stroke="{NAVY}" stroke-width="1.5"/>
  <line x1="{pad_l}" y1="{pad_t + ph}" x2="{pad_l + pw}" y2="{pad_t + ph}" stroke="{NAVY}" stroke-width="1.5"/>
  {''.join(items)}

  <!-- Zone callouts placed inside empty bar-free quadrants -->
  <rect x="{pad_l + pw - 238}" y="{pad_t + 12}" width="224" height="24" rx="4" fill="#FFFFFF" fill-opacity="0.92" stroke="{ORANGE}" stroke-width="1"/>
  <text x="{pad_l + pw - 22}" y="{pad_t + 28}" text-anchor="end" font-size="10.5" font-weight="700" fill="{ORANGE}">Direct-MCP Favored (&gt; 1.0x at small N)</text>

  <rect x="{pad_l + 12}" y="{pad_t + ph - 34}" width="256" height="24" rx="4" fill="#FFFFFF" fill-opacity="0.92" stroke="{TEAL}" stroke-width="1"/>
  <text x="{pad_l + 22}" y="{pad_t + ph - 18}" text-anchor="start" font-size="10.5" font-weight="700" fill="{TEAL}">GAP Favored (&lt; 1.0x · up to 209x win)</text>

  <text x="20" y="{pad_t + ph/2}" text-anchor="middle" font-size="11.5" font-weight="700" fill="{NAVY}" transform="rotate(-90 20 {pad_t + ph/2})">GAP / Direct Token Ratio (log10)</text>
</svg>"""

    def ytex(val: float) -> float:
        return 2.0 + math.log10(max(val, 0.003)) * 0.72

    tex_pts = []
    y0 = ytex(1.0)
    for i, lbl in enumerate(labels):
        cx = i * 1.35 + 0.7
        for ratio, col, dx in ((uncached[i], "blue!75!black", -0.26), (cached[i], "orange!85!black", 0.04)):
            yt = ytex(ratio)
            lo, hi = min(yt, y0), max(yt, y0)
            tex_pts.append(f"  \\fill[{col}] ({cx + dx:.2f},{lo:.3f}) rectangle ({cx + dx + 0.22:.2f},{hi:.3f});")
        l1, l2 = lbl.split("\n")
        tex_pts.append(f"  \\node[below, font=\\tiny, align=center] at ({cx:.2f},0) {{{l1}\\\\{l2}}};")

    tex = "\n".join([
        r"\begin{tikzpicture}[x=0.95cm, y=0.95cm]",
        f"  \\fill[red!5] (0,{y0:.3f}) rectangle (12.4,3.15);",
        f"  \\fill[teal!5] (0,0) rectangle (12.4,{y0:.3f});",
        r"  \draw[->] (0,0) -- (12.5,0);",
        r"  \draw[->] (0,0) -- (0,3.2) node[above, font=\scriptsize] {GAP / Direct Token Ratio ($\log_{10}$)};",
        f"  \\draw[dashed, thick] (0,{y0:.3f}) node[left, font=\\tiny] {{1.0x}} -- (12.4,{y0:.3f});",
        *[
            f"  \\draw[dotted, gray!50] (0,{ytex(v):.3f}) node[left, font=\\tiny, text=black] {{{t}}} -- (12.4,{ytex(v):.3f});"
            for v, t in ((0.01, "0.01x"), (0.1, "0.1x"), (10.0, "10x"))
        ],
        *tex_pts,
        r"  \fill[blue!75!black] (0.4,3.30) rectangle (0.65,3.45) node[right, xshift=3pt, yshift=-2pt, font=\tiny, text=black] {Uncached Ratio};",
        r"  \fill[orange!85!black] (3.4,3.30) rectangle (3.65,3.45) node[right, xshift=3pt, yshift=-2pt, font=\tiny, text=black] {Cached Ratio (0.10x prefix)};",
        r"\end{tikzpicture}",
    ])
    return svg, tex


def generate_fig4_runtime_latencies(m3: dict, s1: dict, ga_local: dict) -> tuple[str, str]:
    ewma_ns = m3["benchmarks"]["gap_balance_percentiles"]["ewma_update_1t_ns"]["net"]["p50"]
    select_ns = m3["benchmarks"]["gap_balance_percentiles"]["select_n2_1t_ns"]["net"]["p50"]
    guard_ns = m3["benchmarks"]["gap_balance_percentiles"]["in_flight_guard_1t_ns"]["net"]["p50"]
    baseline_ms = s1["baseline"]["test_metrics"]["p95_latency_ms"]
    baseline_top1 = s1["baseline"]["test_metrics"]["top1"]["estimate"] * 100.0
    s1_deb_ms = s1["s1_debiased"]["test_metrics"]["p95_latency_ms"]
    s1_deb_top1 = s1["s1_debiased"]["test_metrics"]["top1"]["estimate"] * 100.0
    http2_p95_ms = ga_local["live_runtime_drills"]["p95_ms"]
    http2_rps = ga_local["live_runtime_drills"]["measured_rps"]
    cold_ms = ga_local["live_runtime_drills"]["cold_start_ms"]
    idle_rss_mib = ga_local["live_runtime_drills"]["idle_rss_mib"]
    recov_ms = ga_local["live_runtime_drills"]["sigkill_recovery_ready_ms"]
    h3 = ga_local["http3_live_drills"]
    http3_p95_ms = h3["p95_ms"]
    http3_rps = h3["measured_rps"]

    entries = [
        ("ArcSwap::load (M0)", 15.0, BLUE, "15 ns"),
        ("In-Flight RAII Guard", guard_ns, BLUE, f"{guard_ns:.0f} ns"),
        ("EWMA Update (CAS)", ewma_ns, BLUE, f"{ewma_ns:.0f} ns"),
        ("P2C N=2 Select", select_ns, BLUE, f"{select_ns:.0f} ns"),
        ("Baseline Route p95", baseline_ms * 1e6, TEAL, f"{baseline_ms:.3f} ms ({baseline_top1:.1f}% top-1)"),
        ("HTTP/3 QUIC Mixed p95", http3_p95_ms * 1e6, TEAL, f"{http3_p95_ms:.2f} ms ({http3_rps:,.0f} req/s, 8 conn)"),
        ("HTTP/2 TCP Mixed p95", http2_p95_ms * 1e6, TEAL, f"{http2_p95_ms:.2f} ms ({http2_rps:,.0f} req/s, urllib)"),
        ("SIGKILL Recover /readyz", recov_ms * 1e6, PURPLE, f"{recov_ms:.2f} ms"),
        ("Laya S1 Debiased p95", s1_deb_ms * 1e6, ORANGE, f"{s1_deb_ms:.1f} ms ({s1_deb_top1:.1f}% top-1)"),
        ("Cold Start /readyz", cold_ms * 1e6, PURPLE, f"{cold_ms:.0f} ms ({idle_rss_mib:.1f} MiB RSS)"),
    ]

    w, h = 880, 448
    pad_l, pad_r, pad_t, pad_b = 198, 195, 66, 54
    pw = w - pad_l - pad_r
    ph = h - pad_t - pad_b
    x_min_log, x_max_log = 0.75, 9.1

    def x_px(ns_val: float) -> float:
        lv = math.log10(max(ns_val, 10.0))
        return pad_l + pw * (lv - x_min_log) / (x_max_log - x_min_log)

    grid = []
    for exp, lbl in (
        (1, "10 ns"),
        (2, "100 ns"),
        (3, "1 µs"),
        (4, "10 µs"),
        (5, "100 µs"),
        (6, "1 ms"),
        (7, "10 ms"),
        (8, "100 ms"),
        (9, "1 s"),
    ):
        gx = x_px(10**exp)
        grid.append(f'<line x1="{gx:.1f}" y1="{pad_t}" x2="{gx:.1f}" y2="{pad_t + ph}" stroke="{BORDER}" stroke-dasharray="3 3"/>')
        grid.append(f'<text x="{gx:.1f}" y="{pad_t + ph + 18}" text-anchor="middle" font-size="10" fill="{SLATE}">{esc_xml(lbl)}</text>')

    bars = []
    row_h = ph / len(entries)
    for idx, (name, ns_val, col, val_lbl) in enumerate(entries):
        cy = pad_t + row_h * (idx + 0.5)
        x_end = x_px(ns_val)
        bw = max(6.0, x_end - pad_l)
        bars.append(f'<text x="{pad_l - 10}" y="{cy + 4:.1f}" text-anchor="end" font-size="11" font-weight="600" fill="{NAVY}">{esc_xml(name)}</text>')
        bars.append(f'<rect x="{pad_l}" y="{cy - 9:.1f}" width="{bw:.1f}" height="18" fill="{col}" rx="3"/>')
        bars.append(f'<text x="{pad_l + bw + 8:.1f}" y="{cy + 4:.1f}" font-size="10.2" font-weight="700" fill="{col}">{esc_xml(val_lbl)}</text>')

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" font-family="Helvetica, Arial, sans-serif">
  <rect x="0" y="0" width="{w}" height="{h}" fill="#FFFFFF"/>
  <!-- Category Legend Strip above axes -->
  <rect x="24" y="14" width="{w - 48}" height="32" rx="5" fill="{LIGHT_BG}" stroke="{BORDER}"/>
  <rect x="38" y="24" width="11" height="11" fill="{BLUE}" rx="2"/>
  <text x="54" y="34" font-size="10.5" font-weight="600" fill="{NAVY}">Lock-Free RCU / Balance (ns)</text>
  <rect x="248" y="24" width="11" height="11" fill="{TEAL}" rx="2"/>
  <text x="264" y="34" font-size="10.5" font-weight="600" fill="{NAVY}">Deterministic Route &amp; HTTP/2 + HTTP/3 Load (ms)</text>
  <rect x="572" y="24" width="11" height="11" fill="{PURPLE}" rx="2"/>
  <text x="588" y="34" font-size="10.5" font-weight="600" fill="{NAVY}">Lifecycle (ms)</text>
  <rect x="698" y="24" width="11" height="11" fill="{ORANGE}" rx="2"/>
  <text x="714" y="34" font-size="10.5" font-weight="600" fill="{NAVY}">Un-finetuned S1 (ms)</text>

  {''.join(grid)}
  <line x1="{pad_l}" y1="{pad_t}" x2="{pad_l}" y2="{pad_t + ph}" stroke="{NAVY}" stroke-width="1.5"/>
  <line x1="{pad_l}" y1="{pad_t + ph}" x2="{w - 30}" y2="{pad_t + ph}" stroke="{NAVY}" stroke-width="1.5"/>
  {''.join(bars)}
  <text x="{pad_l + pw/2}" y="{h - 12}" text-anchor="middle" font-size="11" font-weight="700" fill="{NAVY}">Measured Latency Spectrum (log10 nanoseconds · Release &amp; Offline Drills)</text>
</svg>"""

    tex_rows = []
    for idx, (name, ns_val, _col, val_lbl) in enumerate(entries):
        y = (len(entries) - 1 - idx) * 0.44 + 0.35
        x_val = (math.log10(max(ns_val, 10.0)) - 0.75) * 1.05
        tcol = "blue!75!black" if ns_val < 1000 else ("orange!80!black" if "Laya" in name else "teal!75!black")
        tex_rows.append(
            f"  \\node[anchor=east, font=\\tiny] at (0,{y:.2f}) {{{esc_tex(name)}}};\n"
            f"  \\fill[{tcol}] (0,{y - 0.13:.2f}) rectangle ({x_val:.2f},{y + 0.13:.2f}) "
            f"node[right, xshift=3pt, yshift=-3pt, font=\\tiny, text=black] {{{esc_tex(val_lbl)}}};"
        )
    tex = "\n".join([
        r"\begin{tikzpicture}[x=1cm, y=1cm]",
        r"  \draw[->] (0,0) -- (0,4.85);",
        r"  \draw[->] (0,0) -- (9.6,0) node[below, pos=0.5, yshift=-10pt, font=\scriptsize] {Measured Latency ($\log_{10}$ scale)};",
        *[
            f"  \\draw[dotted, gray!45] ({(exp - 0.75) * 1.05:.2f},0) node[below, font=\\tiny, text=black] {{{lbl}}} -- ({(exp - 0.75) * 1.05:.2f},4.75);"
            for exp, lbl in ((1, "10ns"), (3, "$1\\mu\\mathrm{s}$"), (6, "1ms"), (9, "1s"))
        ],
        *tex_rows,
        r"\end{tikzpicture}",
    ])
    return svg, tex


def main() -> None:
    FIG_DIR.mkdir(parents=True, exist_ok=True)
    gapbench = json.loads((ROOT / "bench/results/gapbench/baseline.json").read_text(encoding="utf-8"))
    m3 = json.loads((ROOT / "bench/results/perf/m3-baseline.json").read_text(encoding="utf-8"))
    s1 = json.loads((ROOT / "bench/results/routebench/s1_comparison/s1_comparison.json").read_text(encoding="utf-8"))
    ga_local = json.loads((ROOT / "bench/results/ga_local/report.json").read_text(encoding="utf-8"))

    outputs = {
        "fig1_architecture": generate_fig1_architecture(),
        "fig2_schema_scaling": generate_fig2_schema_scaling(gapbench),
        "fig3_crossover_regimes": generate_fig3_crossover_regimes(gapbench),
        "fig4_runtime_latencies": generate_fig4_runtime_latencies(m3, s1, ga_local),
    }
    for stem, (svg, tex) in outputs.items():
        (FIG_DIR / f"{stem}.svg").write_text(svg, encoding="utf-8")
        (FIG_DIR / f"{stem}.tex").write_text(tex + "\n", encoding="utf-8")
    print(f"Generated {len(outputs) * 2} figure files in {FIG_DIR}")


if __name__ == "__main__":
    main()
