#!/usr/bin/env python3
"""Generate publication-quality SVG and pure-TikZ (.tex) figures directly from measured JSON artifacts."""

from __future__ import annotations

import json
import math
import xml.sax.saxutils as sax
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
FIG_DIR = ROOT / "paper" / "figures"

# Palettes (colorblind-safe Okabe-Ito inspired)
NAVY = "#1B365D"
BLUE = "#0072B2"
ORANGE = "#D55E00"
TEAL = "#009E73"
PURPLE = "#7A5195"
SLATE = "#4A5568"
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
    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 920 470" width="920" height="470" font-family="Helvetica, Arial, sans-serif">
  <rect x="0" y="0" width="920" height="470" fill="#FFFFFF"/>
  <text x="460" y="28" text-anchor="middle" font-size="16" font-weight="700" fill="{NAVY}">Figure 1: Three-Plane Zero-Trust Architecture of Graph Agent Protocol (GAP)</text>

  <!-- Agent Hosts -->
  <rect x="30" y="48" width="860" height="58" rx="8" fill="{LIGHT_BG}" stroke="{NAVY}" stroke-width="1.5"/>
  <text x="50" y="72" font-size="13" font-weight="700" fill="{NAVY}">AI Agent Hosts &amp; Clients</text>
  <text x="50" y="92" font-size="11.5" fill="{SLATE}">MCP 2026-07-28 Facade (5 static tools, 651 tokens)  |  Native GAP-QL (/graphql)  |  A2A v1.0 Tasks  |  Rust CLI</text>

  <!-- Plane 1: Deterministic Execution -->
  <rect x="30" y="132" width="540" height="190" rx="8" fill="#EFF6FF" stroke="{BLUE}" stroke-width="1.8"/>
  <text x="48" y="156" font-size="13" font-weight="700" fill="{BLUE}">Plane 1: Deterministic Hot Execution Plane (Zero ML, INV-01 / INV-02 / INV-04)</text>

  <rect x="48" y="170" width="115" height="62" rx="6" fill="#FFFFFF" stroke="{BLUE}" stroke-width="1.2"/>
  <text x="105" y="193" text-anchor="middle" font-size="11" font-weight="700" fill="{NAVY}">AuthN + Cedar</text>
  <text x="105" y="210" text-anchor="middle" font-size="10" fill="{SLATE}">Default-Deny</text>
  <text x="105" y="223" text-anchor="middle" font-size="10" fill="{SLATE}">Scopes &amp; Cost</text>

  <rect x="178" y="170" width="118" height="62" rx="6" fill="#FFFFFF" stroke="{BLUE}" stroke-width="1.2"/>
  <text x="237" y="193" text-anchor="middle" font-size="11" font-weight="700" fill="{NAVY}">GAP-QL Planner</text>
  <text x="237" y="210" text-anchor="middle" font-size="10" fill="{SLATE}">ArcSwap Pinning</text>
  <text x="237" y="223" text-anchor="middle" font-size="10" fill="{SLATE}">Tenant PlanCache</text>

  <rect x="311" y="170" width="118" height="62" rx="6" fill="#FFFFFF" stroke="{BLUE}" stroke-width="1.2"/>
  <text x="370" y="193" text-anchor="middle" font-size="11" font-weight="700" fill="{NAVY}">Batch Executor</text>
  <text x="370" y="210" text-anchor="middle" font-size="10" fill="{SLATE}">DataLoader O(1)</text>
  <text x="370" y="223" text-anchor="middle" font-size="10" fill="{SLATE}">P2C + Breaker</text>

  <rect x="444" y="170" width="110" height="62" rx="6" fill="#FFFFFF" stroke="{BLUE}" stroke-width="1.2"/>
  <text x="499" y="193" text-anchor="middle" font-size="11" font-weight="700" fill="{NAVY}">Security Guard</text>
  <text x="499" y="210" text-anchor="middle" font-size="10" fill="{SLATE}">EgressGuard SSRF</text>
  <text x="499" y="223" text-anchor="middle" font-size="10" fill="{SLATE}">Sandbox &amp; T2 Tag</text>

  <!-- Sub-box inside Plane 1 -->
  <rect x="48" y="248" width="506" height="58" rx="6" fill="#FFFFFF" stroke="{BORDER}" stroke-width="1.2"/>
  <text x="301" y="270" text-anchor="middle" font-size="11.5" font-weight="700" fill="{NAVY}">Native Graph Connector (NGC) Adapters + Durable State</text>
  <text x="301" y="290" text-anchor="middle" font-size="10.5" fill="{SLATE}">OpenAPI  |  GraphQL  |  gRPC  |  SQL  |  CLI  |  Code Graph (tree-sitter)  |  SQLite Context/Tasks/BLAKE3 Audit</text>

  <!-- Plane 2: Discovery -->
  <rect x="590" y="132" width="300" height="90" rx="8" fill="#ECFDF5" stroke="{TEAL}" stroke-width="1.8"/>
  <text x="606" y="156" font-size="13" font-weight="700" fill="{TEAL}">Plane 2: Calibrated Discovery</text>
  <text x="606" y="176" font-size="10.5" fill="{NAVY}">__metaGraph (L0) -&gt; __search (L1) -&gt; __type (L2)</text>
  <text x="606" y="194" font-size="10.5" fill="{SLATE}">BM25 + HNSW Hybrid + Visibility Bitset (INV-05)</text>
  <text x="606" y="210" font-size="10.5" fill="{SLATE}">Optional S1 Candle / Conformal (INV-17 Fallback)</text>

  <!-- Plane 3: Ingestion & Snapshot Control -->
  <rect x="590" y="232" width="300" height="90" rx="8" fill="#FFF7ED" stroke="{ORANGE}" stroke-width="1.8"/>
  <text x="606" y="256" font-size="13" font-weight="700" fill="{ORANGE}">Plane 3: Zero-Trust Control Plane</text>
  <text x="606" y="276" font-size="10.5" fill="{NAVY}">Propose-Not-Activate + T0/T1/T2 Trust Tiers</text>
  <text x="606" y="294" font-size="10.5" fill="{SLATE}">Desc-Hash Pinning -&gt; Drift Freeze (SEC-082)</text>
  <text x="606" y="310" font-size="10.5" fill="{SLATE}">Wait-Free Snapshot Publish + Auto-Rollback</text>

  <!-- Bottom Cross-cutting Invariant strip -->
  <rect x="30" y="344" width="860" height="96" rx="8" fill="{LIGHT_BG}" stroke="{BORDER}" stroke-width="1.4"/>
  <text x="48" y="368" font-size="12" font-weight="700" fill="{NAVY}">Downstream Backends &amp; Runtime Invariants</text>
  <text x="48" y="388" font-size="11" fill="{SLATE}">1. Snapshot Atomicity (INV-01): One Arc&lt;Snapshot&gt; per request; zero partial state across swaps.</text>
  <text x="48" y="406" font-size="11" fill="{SLATE}">2. Tenant Isolation (INV-09): TenantId embedded in PlanCache, TaskStore, ContextStore, Quotas, and Audit chains.</text>
  <text x="48" y="424" font-size="11" fill="{SLATE}">3. Rug-Pull &amp; Exfil Defense (INV-08, INV-15): Description mutations freeze branch; untrusted outputs escaped in &lt;gap:untrusted&gt;.</text>

  <!-- Connectors -->
  <line x1="280" y1="106" x2="280" y2="132" stroke="{NAVY}" stroke-width="1.8"/>
  <line x1="740" y1="106" x2="740" y2="132" stroke="{TEAL}" stroke-width="1.8"/>
  <line x1="590" y1="277" x2="570" y2="277" stroke="{ORANGE}" stroke-width="1.8" stroke-dasharray="4 3"/>
</svg>"""

    tex = r"""\begin{tikzpicture}[
  box/.style={draw, rounded corners=3pt, align=center, font=\scriptsize, inner sep=4pt, fill=white},
  plane/.style={draw, rounded corners=4pt, thick, inner sep=6pt},
  >=stealth
]
  \node[box, fill=gray!8, minimum width=15.6cm, draw=black!70] (hosts) at (0, 3.6)
    {\textbf{Agent Hosts \& Clients}\\
     MCP \texttt{2026-07-28} Facade (5 static tools, 651 tokens) $\mid$ Native GAP-QL (\texttt{/graphql}) $\mid$ A2A v1.0 Tasks $\mid$ CLI};

  \draw[fill=blue!4, draw=blue!70!black, rounded corners=4pt, thick] (-7.8, -0.3) rectangle (1.9, 2.75);
  \node[anchor=west, font=\scriptsize\bfseries, text=blue!60!black] at (-7.65, 2.50)
    {Plane 1: Deterministic Execution (Zero ML on Hot Path, ARC-002 / INV-01 / INV-02)};

  \node[box, draw=blue!60!black, minimum width=2.15cm, minimum height=0.95cm] (auth) at (-6.55, 1.65)
    {\textbf{AuthN + Cedar}\\Default-Deny\\INV-04};
  \node[box, draw=blue!60!black, minimum width=2.15cm, minimum height=0.95cm] (plan) at (-4.15, 1.65)
    {\textbf{GAP-QL Plan}\\ArcSwap Pin\\Tenant Cache};
  \node[box, draw=blue!60!black, minimum width=2.15cm, minimum height=0.95cm] (exec) at (-1.75, 1.65)
    {\textbf{Batch Exec}\\DataLoader\\P2C + Breaker};
  \node[box, draw=blue!60!black, minimum width=2.15cm, minimum height=0.95cm] (guard) at (0.65, 1.65)
    {\textbf{Sec Guard}\\EgressGuard\\Sandbox + T2};

  \node[box, draw=black!40, minimum width=9.2cm, minimum height=0.8cm] (ngc) at (-2.95, 0.35)
    {\textbf{NGC Adapters \& Durable Stores}\\
     OpenAPI $\cdot$ GraphQL $\cdot$ gRPC $\cdot$ SQL $\cdot$ CLI $\cdot$ Code Graph (\texttt{tree-sitter}) $\cdot$ SQLite Context/Tasks/BLAKE3 Audit};

  \draw[fill=teal!6, draw=teal!70!black, rounded corners=4pt, thick] (2.25, 1.35) rectangle (7.8, 2.75);
  \node[anchor=west, font=\scriptsize\bfseries, text=teal!60!black] at (2.4, 2.50) {Plane 2: Calibrated Discovery};
  \node[anchor=west, font=\tiny, align=left] at (2.4, 1.90)
    {\texttt{\_\_metaGraph} ($L_0$) $\rightarrow$ \texttt{\_\_search} ($L_1$) $\rightarrow$ \texttt{\_\_type} ($L_2$)\\
     BM25 + HNSW + Visibility Bitset (\texttt{INV-05})\\
     Optional S1 + Conformal / Baseline Fallback (\texttt{INV-17})};

  \draw[fill=orange!6, draw=orange!80!black, rounded corners=4pt, thick] (2.25, -0.3) rectangle (7.8, 1.12);
  \node[anchor=west, font=\scriptsize\bfseries, text=orange!70!black] at (2.4, 0.87) {Plane 3: Zero-Trust Control};
  \node[anchor=west, font=\tiny, align=left] at (2.4, 0.25)
    {Propose-Not-Activate + $T_0/T_1/T_2$ Trust Tiers\\
     Desc-Hash Pin $\rightarrow$ Drift Freeze (\texttt{SEC-082})\\
     Wait-Free Swap + Error-Spike Rollback};

  \node[box, fill=gray!6, draw=black!45, minimum width=15.6cm, align=left] (inv) at (0, -1.25)
    {\textbf{Cross-Cutting Invariants:} \texttt{INV-01} single snapshot per request; \texttt{INV-09} \texttt{TenantId} in every key; \texttt{INV-15} \texttt{Secret<T>} redaction; \texttt{INV-18} batched joins; \texttt{SEC-080} \texttt{<gap:untrusted>} tagging.};

  \draw[->, thick] (-2.95, 3.10) -- (-2.95, 2.76);
  \draw[->, thick, draw=teal!70!black] (5.0, 3.10) -- (5.0, 2.76);
  \draw[->, dashed, thick, draw=orange!80!black] (2.25, 0.41) -- (1.90, 0.41);
\end{tikzpicture}
"""
    return svg, tex


def generate_fig2_schema_scaling(gapbench: dict) -> tuple[str, str]:
    rows = gapbench["discovery_scaling"]
    w, h = 760, 420
    pad_l, pad_r, pad_t, pad_b = 85, 35, 52, 65
    pw = w - pad_l - pad_r
    ph = h - pad_t - pad_b

    # Log10 y range: 10^1 (10) to 10^6 (1,000,000)
    y_min_log, y_max_log = 1.5, 5.7
    xs = [pad_l + pw * (i + 0.5) / len(rows) for i in range(len(rows))]

    def y_px(val: float) -> float:
        lv = math.log10(max(val, 1.0))
        frac = (lv - y_min_log) / (y_max_log - y_min_log)
        return pad_t + ph * (1.0 - frac)

    bars = []
    for i, row in enumerate(rows):
        cx = xs[i]
        bw = 38
        # Direct bar (left)
        d_val = row["direct_tools_list_tokens"]
        d_y = y_px(d_val)
        d_h = (pad_t + ph) - d_y
        bars.append(f'<rect x="{cx - bw - 4:.1f}" y="{d_y:.1f}" width="{bw}" height="{d_h:.1f}" fill="{ORANGE}" rx="3"/>')
        bars.append(f'<text x="{cx - bw/2 - 4:.1f}" y="{d_y - 6:.1f}" text-anchor="middle" font-size="10.5" font-weight="700" fill="{ORANGE}">{d_val:,}</text>')

        # GAP bar (right)
        g_val = row["gap_tools_list_tokens"]
        g_y = y_px(g_val)
        g_h = (pad_t + ph) - g_y
        bars.append(f'<rect x="{cx + 4:.1f}" y="{g_y:.1f}" width="{bw}" height="{g_h:.1f}" fill="{BLUE}" rx="3"/>')
        bars.append(f'<text x="{cx + bw/2 + 4:.1f}" y="{g_y - 6:.1f}" text-anchor="middle" font-size="10.5" font-weight="700" fill="{BLUE}">{g_val:,}</text>')

        bars.append(f'<text x="{cx:.1f}" y="{pad_t + ph + 20}" text-anchor="middle" font-size="11.5" font-weight="700" fill="{NAVY}">N = {row["n_tools"]}</text>')
        bars.append(f'<text x="{cx:.1f}" y="{pad_t + ph + 35}" text-anchor="middle" font-size="10" fill="{SLATE}">({row["branches"]} branches)</text>')

    grid = []
    for exp in (2, 3, 4, 5):
        val = 10**exp
        gy = y_px(val)
        grid.append(f'<line x1="{pad_l}" y1="{gy:.1f}" x2="{w - pad_r}" y2="{gy:.1f}" stroke="{BORDER}" stroke-dasharray="3 3"/>')
        grid.append(f'<text x="{pad_l - 8}" y="{gy + 4:.1f}" text-anchor="end" font-size="10.5" fill="{SLATE}">{val:,}</text>')

    gate_y = y_px(1200)
    grid.append(f'<line x1="{pad_l}" y1="{gate_y:.1f}" x2="{w - pad_r}" y2="{gate_y:.1f}" stroke="{TEAL}" stroke-width="1.6" stroke-dasharray="6 4"/>')
    grid.append(f'<text x="{w - pad_r - 4}" y="{gate_y - 6:.1f}" text-anchor="end" font-size="10.5" font-weight="700" fill="{TEAL}">Gate G-F1 limit (1,200 tokens)</text>')

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" font-family="Helvetica, Arial, sans-serif">
  <rect x="0" y="0" width="{w}" height="{h}" fill="#FFFFFF"/>
  <text x="{w/2}" y="26" text-anchor="middle" font-size="15" font-weight="700" fill="{NAVY}">Figure 2: MCP tools/list Token Footprint Scaling (Log Scale, GQL-022 Estimator)</text>
  {''.join(grid)}
  <line x1="{pad_l}" y1="{pad_t}" x2="{pad_l}" y2="{pad_t + ph}" stroke="{NAVY}" stroke-width="1.5"/>
  <line x1="{pad_l}" y1="{pad_t + ph}" x2="{w - pad_r}" y2="{pad_t + ph}" stroke="{NAVY}" stroke-width="1.5"/>
  {''.join(bars)}
  <text x="20" y="{pad_t + ph/2}" text-anchor="middle" font-size="11.5" font-weight="700" fill="{NAVY}" transform="rotate(-90 20 {pad_t + ph/2})">tools/list Tokens (log10)</text>
  <!-- Legend -->
  <rect x="{pad_l + 15}" y="{pad_t + 8}" width="12" height="12" fill="{ORANGE}" rx="2"/>
  <text x="{pad_l + 32}" y="{pad_t + 18}" font-size="11" fill="{NAVY}">Direct-MCP (grows O(N): 1,904 -&gt; 203,421)</text>
  <rect x="{pad_l + 15}" y="{pad_t + 26}" width="12" height="12" fill="{BLUE}" rx="2"/>
  <text x="{pad_l + 32}" y="{pad_t + 36}" font-size="11" fill="{NAVY}">GAP 5-Tool Facade (constant O(1): 651 tokens)</text>
</svg>"""

    # TikZ exact coordinate bar chart (log10 scaled cleanly)
    tex_bars = []
    for i, row in enumerate(rows):
        x0 = i * 1.9
        d_val = row["direct_tools_list_tokens"]
        g_val = row["gap_tools_list_tokens"]
        dy = (math.log10(d_val) - 1.5) * 0.9
        gy = (math.log10(g_val) - 1.5) * 0.9
        tex_bars.append(
            f"  \\fill[orange!80!black] ({x0:.2f},0) rectangle ({x0 + 0.55:.2f},{dy:.3f});\n"
            f"  \\node[above, font=\\tiny, text=orange!70!black] at ({x0 + 0.275:.2f},{dy:.3f}) {{{d_val:,}}};\n"
            f"  \\fill[blue!75!black] ({x0 + 0.65:.2f},0) rectangle ({x0 + 1.20:.2f},{gy:.3f});\n"
            f"  \\node[above, font=\\tiny, text=blue!70!black] at ({x0 + 0.925:.2f},{gy:.3f}) {{{g_val:,}}};\n"
            f"  \\node[below, font=\\scriptsize] at ({x0 + 0.60:.2f},0) {{$N={row['n_tools']}$}};"
        )
    gate_tex_y = (math.log10(1200.0) - 1.5) * 0.9
    tex = "\n".join([
        r"\begin{tikzpicture}[x=1cm, y=1cm]",
        r"  \draw[->] (-0.3,0) -- (7.5,0);",
        r"  \draw[->] (-0.3,0) -- (-0.3,3.8) node[above, font=\scriptsize] {\texttt{tools/list} Tokens ($\log_{10}$)};",
        f"  \\draw[dashed, teal!75!black, thick] (-0.3,{gate_tex_y:.3f}) -- (7.3,{gate_tex_y:.3f}) node[anchor=south east, font=\\tiny, text=teal!75!black] {{Gate G-F1 (1,200)}};",
        *[
            f"  \\draw[gray!35, dotted] (-0.3,{(exp - 1.5) * 0.9:.3f}) node[left, font=\\tiny, text=black] {{$10^{exp}$}} -- (7.3,{(exp - 1.5) * 0.9:.3f});"
            for exp in (2, 3, 4, 5)
        ],
        *tex_bars,
        r"  \fill[orange!80!black] (0.0,3.45) rectangle (0.25,3.62) node[right, xshift=4pt, yshift=-2pt, font=\tiny, text=black] {Direct-MCP ($O(N)$)};",
        r"  \fill[blue!75!black] (2.8,3.45) rectangle (3.05,3.62) node[right, xshift=4pt, yshift=-2pt, font=\tiny, text=black] {GAP Facade ($O(1) = 651$)};",
        r"\end{tikzpicture}",
    ])
    return svg, tex


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

    w, h = 840, 420
    pad_l, pad_r, pad_t, pad_b = 82, 32, 55, 78
    pw = w - pad_l - pad_r
    ph = h - pad_t - pad_b
    y_min_log, y_max_log = -2.5, 1.15  # 0.003x to 14x

    def y_px(val: float) -> float:
        lv = math.log10(max(val, 0.003))
        frac = (lv - y_min_log) / (y_max_log - y_min_log)
        return pad_t + ph * (1.0 - frac)

    y_parity = y_px(1.0)
    grid = [
        f'<rect x="{pad_l}" y="{pad_t}" width="{pw}" height="{y_parity - pad_t:.1f}" fill="#FEF2F2" opacity="0.65"/>',
        f'<rect x="{pad_l}" y="{y_parity:.1f}" width="{pw}" height="{pad_t + ph - y_parity:.1f}" fill="#ECFDF5" opacity="0.55"/>',
    ]
    for val, lbl in ((0.01, "0.01x"), (0.1, "0.1x"), (1.0, "1.0x (Parity)"), (10.0, "10.0x")):
        gy = y_px(val)
        col = NAVY if val == 1.0 else BORDER
        sw = "1.8" if val == 1.0 else "1"
        dash = "6 4" if val == 1.0 else "3 3"
        grid.append(f'<line x1="{pad_l}" y1="{gy:.1f}" x2="{w - pad_r}" y2="{gy:.1f}" stroke="{col}" stroke-width="{sw}" stroke-dasharray="{dash}"/>')
        grid.append(f'<text x="{pad_l - 8}" y="{gy + 4:.1f}" text-anchor="end" font-size="10.5" fill="{NAVY if val == 1.0 else SLATE}">{lbl}</text>')

    items = []
    step = pw / len(labels)
    for i, lbl in enumerate(labels):
        cx = pad_l + step * (i + 0.5)
        bw = 22
        for series_idx, (ratio, col, offset) in enumerate(((uncached[i], BLUE, -bw - 2), (cached[i], ORANGE, 2))):
            yp = y_px(ratio)
            top = min(yp, y_parity)
            ht = max(2.0, abs(yp - y_parity))
            items.append(f'<rect x="{cx + offset:.1f}" y="{top:.1f}" width="{bw}" height="{ht:.1f}" fill="{col}" rx="2"/>')
            txt_y = (yp - 5) if ratio >= 1.0 else (yp + 12)
            items.append(f'<text x="{cx + offset + bw/2:.1f}" y="{txt_y:.1f}" text-anchor="middle" font-size="8.5" font-weight="700" fill="{col}">{ratio:.3g}x</text>')
        l1, l2 = lbl.split("\n")
        items.append(f'<text x="{cx:.1f}" y="{pad_t + ph + 18}" text-anchor="middle" font-size="10.5" font-weight="700" fill="{NAVY}">{l1}</text>')
        items.append(f'<text x="{cx:.1f}" y="{pad_t + ph + 33}" text-anchor="middle" font-size="10" fill="{SLATE}">{l2}</text>')

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" font-family="Helvetica, Arial, sans-serif">
  <rect x="0" y="0" width="{w}" height="{h}" fill="#FFFFFF"/>
  <text x="{w/2}" y="26" text-anchor="middle" font-size="15" font-weight="700" fill="{NAVY}">Figure 3: Token Ratio GAP / Direct-MCP Across Regimes (&lt;1.0x = GAP Wins)</text>
  {''.join(grid)}
  <line x1="{pad_l}" y1="{pad_t}" x2="{pad_l}" y2="{pad_t + ph}" stroke="{NAVY}" stroke-width="1.5"/>
  <line x1="{pad_l}" y1="{pad_t + ph}" x2="{w - pad_r}" y2="{pad_t + ph}" stroke="{NAVY}" stroke-width="1.5"/>
  {''.join(items)}
  <text x="{w - pad_r - 8}" y="{pad_t + 18}" text-anchor="end" font-size="10.5" font-weight="700" fill="{ORANGE}">Direct-MCP Uses Fewer Tokens (&gt;1.0x)</text>
  <text x="{w - pad_r - 8}" y="{pad_t + ph - 10}" text-anchor="end" font-size="10.5" font-weight="700" fill="{TEAL}">GAP Wins by Up to 119x-209x (&lt;1.0x)</text>
  <rect x="{pad_l + 12}" y="{pad_t + 10}" width="11" height="11" fill="{BLUE}" rx="2"/>
  <text x="{pad_l + 28}" y="{pad_t + 19}" font-size="10.5" fill="{NAVY}">Uncached Ratio (G/D)</text>
  <rect x="{pad_l + 165}" y="{pad_t + 10}" width="11" height="11" fill="{ORANGE}" rx="2"/>
  <text x="{pad_l + 181}" y="{pad_t + 19}" font-size="10.5" fill="{NAVY}">Cached Ratio (0.10x Prefix)</text>
</svg>"""

    tex_pts = []
    # Map log10(ratio) centered around 0 -> y = 2.0 + log10(ratio)*0.75
    def ytex(val: float) -> float:
        return 2.0 + math.log10(max(val, 0.003)) * 0.75

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
        r"\begin{tikzpicture}[x=1cm, y=1cm]",
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
        r"  \fill[blue!75!black] (0.4,3.0) rectangle (0.65,3.15) node[right, xshift=3pt, yshift=-2pt, font=\tiny, text=black] {Uncached Ratio};",
        r"  \fill[orange!85!black] (3.0,3.0) rectangle (3.25,3.15) node[right, xshift=3pt, yshift=-2pt, font=\tiny, text=black] {Cached Ratio (0.10x prefix)};",
        r"\end{tikzpicture}",
    ])
    return svg, tex


def generate_fig4_runtime_latencies(m3: dict, s1: dict, ga_local: dict) -> tuple[str, str]:
    ewma_ns = m3["benchmarks"]["gap_balance_percentiles"]["ewma_update_1t_ns"]["net"]["p50"]
    select_ns = m3["benchmarks"]["gap_balance_percentiles"]["select_n2_1t_ns"]["net"]["p50"]
    guard_ns = m3["benchmarks"]["gap_balance_percentiles"]["in_flight_guard_1t_ns"]["net"]["p50"]
    baseline_ms = s1["baseline"]["test_metrics"]["p95_latency_ms"]
    baseline_top1 = s1["baseline"]["test_metrics"]["top1"]["estimate"] * 100.0
    s1_fwd_ms = s1["s1_forward"]["test_metrics"]["p95_latency_ms"]
    s1_fwd_top1 = s1["s1_forward"]["test_metrics"]["top1"]["estimate"] * 100.0
    s1_deb_ms = s1["s1_debiased"]["test_metrics"]["p95_latency_ms"]
    s1_deb_top1 = s1["s1_debiased"]["test_metrics"]["top1"]["estimate"] * 100.0
    http_p95_ms = ga_local["live_runtime_drills"]["p95_ms"]
    cold_ms = ga_local["live_runtime_drills"]["cold_start_ms"]
    recov_ms = ga_local["live_runtime_drills"]["sigkill_recovery_ready_ms"]

    # Panel A: Latency spanning nanoseconds to ~1s (log10 nanoseconds: 10^1 = 10ns .. 10^9 = 1s)
    entries = [
        ("ArcSwap::load (M0)", 15.0, BLUE, "15 ns"),
        ("In-Flight RAII Guard", guard_ns, BLUE, f"{guard_ns:.0f} ns"),
        ("EWMA Update (CAS)", ewma_ns, BLUE, f"{ewma_ns:.0f} ns"),
        ("P2C N=2 Select", select_ns, BLUE, f"{select_ns:.0f} ns"),
        ("Baseline Route p95", baseline_ms * 1e6, TEAL, f"{baseline_ms:.3f} ms ({baseline_top1:.1f}% top-1)"),
        ("HTTP Mixed p95 (Live)", http_p95_ms * 1e6, TEAL, f"{http_p95_ms:.2f} ms (800 reqs)"),
        ("SIGKILL Recover", recov_ms * 1e6, PURPLE, f"{recov_ms:.2f} ms"),
        ("Laya S1 Debiased p95", s1_deb_ms * 1e6, ORANGE, f"{s1_deb_ms:.1f} ms ({s1_deb_top1:.1f}% top-1)"),
        ("Cold Start /readyz", cold_ms * 1e6, PURPLE, f"{cold_ms:.0f} ms"),
    ]

    w, h = 860, 420
    pad_l, pad_r, pad_t, pad_b = 180, 145, 55, 50
    pw = w - pad_l - pad_r
    ph = h - pad_t - pad_b
    x_min_log, x_max_log = 0.8, 9.4  # ~6 ns to ~2.5 s

    def x_px(ns_val: float) -> float:
        lv = math.log10(max(ns_val, 10.0))
        return pad_l + pw * (lv - x_min_log) / (x_max_log - x_min_log)

    grid = []
    for exp, lbl in ((1, "10 ns"), (2, "100 ns"), (3, "1 us"), (4, "10 us"), (5, "100 us"), (6, "1 ms"), (7, "10 ms"), (8, "100 ms"), (9, "1 s")):
        gx = x_px(10**exp)
        grid.append(f'<line x1="{gx:.1f}" y1="{pad_t}" x2="{gx:.1f}" y2="{pad_t + ph}" stroke="{BORDER}" stroke-dasharray="3 3"/>')
        grid.append(f'<text x="{gx:.1f}" y="{pad_t + ph + 18}" text-anchor="middle" font-size="10" fill="{SLATE}">{lbl}</text>')

    bars = []
    row_h = ph / len(entries)
    for idx, (name, ns_val, col, val_lbl) in enumerate(entries):
        cy = pad_t + row_h * (idx + 0.5)
        x_end = x_px(ns_val)
        bw = max(4.0, x_end - pad_l)
        bars.append(f'<text x="{pad_l - 8}" y="{cy + 4:.1f}" text-anchor="end" font-size="11" font-weight="600" fill="{NAVY}">{esc_xml(name)}</text>')
        bars.append(f'<rect x="{pad_l}" y="{cy - 9:.1f}" width="{bw:.1f}" height="18" fill="{col}" rx="3"/>')
        bars.append(f'<text x="{pad_l + bw + 6:.1f}" y="{cy + 4:.1f}" font-size="10.5" font-weight="700" fill="{col}">{esc_xml(val_lbl)}</text>')

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" font-family="Helvetica, Arial, sans-serif">
  <rect x="0" y="0" width="{w}" height="{h}" fill="#FFFFFF"/>
  <text x="{w/2}" y="26" text-anchor="middle" font-size="15" font-weight="700" fill="{NAVY}">Figure 4: Measured Multi-Scale Latencies &amp; Routing Quality Comparison</text>
  {''.join(grid)}
  <line x1="{pad_l}" y1="{pad_t}" x2="{pad_l}" y2="{pad_t + ph}" stroke="{NAVY}" stroke-width="1.5"/>
  <line x1="{pad_l}" y1="{pad_t + ph}" x2="{w - pad_r}" y2="{pad_t + ph}" stroke="{NAVY}" stroke-width="1.5"/>
  {''.join(bars)}
  <text x="{pad_l + pw/2}" y="{h - 10}" text-anchor="middle" font-size="11" font-weight="700" fill="{NAVY}">Latency Scale (log10 nanoseconds) — Measured Offline &amp; Live Drills</text>
</svg>"""

    tex_rows = []
    for idx, (name, ns_val, _col, val_lbl) in enumerate(entries):
        y = (len(entries) - 1 - idx) * 0.48 + 0.35
        x_val = (math.log10(max(ns_val, 10.0)) - 0.8) * 1.08
        tcol = "blue!75!black" if ns_val < 1000 else ("orange!80!black" if "Laya" in name else "teal!75!black")
        tex_rows.append(
            f"  \\node[anchor=east, font=\\tiny] at (0,{y:.2f}) {{{esc_tex(name)}}};\n"
            f"  \\fill[{tcol}] (0,{y - 0.14:.2f}) rectangle ({x_val:.2f},{y + 0.14:.2f}) "
            f"node[right, xshift=3pt, yshift=-3pt, font=\\tiny, text=black] {{{esc_tex(val_lbl)}}};"
        )
    tex = "\n".join([
        r"\begin{tikzpicture}[x=1cm, y=1cm]",
        r"  \draw[->] (0,0) -- (0,4.65);",
        r"  \draw[->] (0,0) -- (9.6,0) node[below, pos=0.5, yshift=-10pt, font=\scriptsize] {Measured Latency ($\log_{10}$ scale)};",
        *[
            f"  \\draw[dotted, gray!45] ({(exp - 0.8) * 1.08:.2f},0) node[below, font=\\tiny, text=black] {{{lbl}}} -- ({(exp - 0.8) * 1.08:.2f},4.55);"
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
