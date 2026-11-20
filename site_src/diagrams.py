"""Accessible localized inline SVG architecture diagrams using CSS theme variables."""
from __future__ import annotations

from .render import esc


def svg_mental_model(lang: str) -> str:
    """Before Direct-MCP vs Unified GAP Supergraph comparison diagram."""
    if lang == "vi":
        title = "So sánh Direct-MCP phẳng và Cổng GAP hợp nhất"
        b_head = "Cách cũ (Direct-MCP phẳng)"
        b_box1 = "Prompt phình 203.421 tokens"
        b_box2 = "1.000 JSON tool tải sẵn mỗi lượt"
        b_arr = "Vòng lặp N+1 qua LLM (4 lượt)"
        b_box3 = "Rủi ro Rug-Pull mô tả · Thiếu Cedar"
        g_head = "Với Cổng GAP (Supergraph định kiểu)"
        g_box1 = "5 MCP Tools tĩnh (cố định 651 tok)"
        g_box2 = "Khám phá L0 -> L1 -> L2 + Cedar"
        g_arr = "DataLoader Batch + ArcSwap (2 lượt)"
        g_box3 = "6 NGC Branches · Chặn SSRF & Drift"
    else:
        title = "Comparison of flat Direct-MCP vs Unified GAP Supergraph"
        b_head = "Without GAP (Flat Direct-MCP)"
        b_box1 = "203,421-Token Prompt Bloat"
        b_box2 = "1,000 raw tool schemas up front"
        b_arr = "Agent N+1 loop (4 turns)"
        b_box3 = "Mutable descriptions (Rug-Pull risk)"
        g_head = "With GAP (Typed Supergraph)"
        g_box1 = "5 Static MCP Tools (Fixed 651 tok)"
        g_box2 = "L0 -> L1 -> L2 Discovery + Cedar AuthZ"
        g_arr = "DataLoader Batch + ArcSwap (2 turns)"
        g_box3 = "6 NGC Branches · SSRF & Drift Guard"

    return f"""<figure class="diagram-frame">
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 860 304" width="860" height="304" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <defs>
      <marker id="mm-arr-warn" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--warn)"/>
      </marker>
      <marker id="mm-arr-ok" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--ok)"/>
      </marker>
    </defs>
    <rect x="12" y="10" width="406" height="284" rx="12" fill="var(--surface-alt)" stroke="var(--warn)" stroke-width="1.5"/>
    <text x="215" y="38" text-anchor="middle" fill="var(--warn)" font-size="14" font-weight="700">{esc(b_head)}</text>
    <rect x="30" y="54" width="370" height="54" rx="8" fill="var(--surface)" stroke="var(--border)"/>
    <text x="215" y="77" text-anchor="middle" fill="var(--text)" font-size="13" font-weight="700">{esc(b_box1)}</text>
    <text x="215" y="96" text-anchor="middle" fill="var(--muted)" font-size="11.5">{esc(b_box2)}</text>
    <line x1="215" y1="108" x2="215" y2="162" stroke="var(--warn)" stroke-width="2" stroke-dasharray="4 3" marker-end="url(#mm-arr-warn)"/>
    <rect x="55" y="121" width="320" height="26" rx="13" fill="var(--surface)" stroke="var(--warn)"/>
    <text x="215" y="138" text-anchor="middle" fill="var(--warn)" font-size="11.5" font-weight="700">{esc(b_arr)}</text>
    <rect x="30" y="168" width="370" height="106" rx="8" fill="var(--surface)" stroke="var(--border)"/>
    <text x="215" y="210" text-anchor="middle" fill="var(--text)" font-size="13" font-weight="600">{esc(b_box3)}</text>
    <text x="215" y="240" text-anchor="middle" fill="var(--muted)" font-size="12">REST  |  SQL  |  gRPC  |  MCP  |  Code AST</text>

    <rect x="442" y="10" width="406" height="284" rx="12" fill="var(--surface-alt)" stroke="var(--accent)" stroke-width="2"/>
    <text x="645" y="38" text-anchor="middle" fill="var(--accent)" font-size="14" font-weight="700">{esc(g_head)}</text>
    <rect x="460" y="54" width="370" height="54" rx="8" fill="var(--surface)" stroke="var(--accent)"/>
    <text x="645" y="77" text-anchor="middle" fill="var(--accent)" font-size="13" font-weight="700">{esc(g_box1)}</text>
    <text x="645" y="96" text-anchor="middle" fill="var(--muted)" font-size="11.5">{esc(g_box2)}</text>
    <line x1="645" y1="108" x2="645" y2="162" stroke="var(--ok)" stroke-width="2" marker-end="url(#mm-arr-ok)"/>
    <rect x="485" y="121" width="320" height="26" rx="13" fill="var(--surface)" stroke="var(--ok)"/>
    <text x="645" y="138" text-anchor="middle" fill="var(--ok)" font-size="11.5" font-weight="700">{esc(g_arr)}</text>
    <rect x="460" y="168" width="370" height="106" rx="8" fill="var(--surface)" stroke="var(--ok)"/>
    <text x="645" y="210" text-anchor="middle" fill="var(--ok)" font-size="13" font-weight="700">{esc(g_box3)}</text>
    <text x="645" y="240" text-anchor="middle" fill="var(--muted)" font-size="12">HTTP/3 QUIC  ·  MCP  ·  A2A v1.0  ·  AG-UI</text>
  </svg>
  <figcaption class="diagram-caption">{esc(title)}</figcaption>
</figure>"""


def svg_three_planes(lang: str) -> str:
    """Three-Plane separation diagram (Execution Zero-ML, Discovery, Control)."""
    if lang == "vi":
        title = "Kiến trúc 3 Mặt phẳng tách biệt hoàn toàn trong GAP"
        p2 = "Mặt phẳng 2: Khám phá Lũy tiến"
        p2s = "L0/L1/L2 · Visibility Bitset (INV-05) · Baseline 95,24% / S1 tùy chọn (INV-17)"
        p1 = "Mặt phẳng 1: Thực thi Tất định (CẤM ML — ARC-002)"
        p1s = "AuthN + Cedar Policy -> PlanCache (TenantId) -> ArcSwap Snapshot -> BatchJoin -> EgressGuard"
        p3 = "Mặt phẳng 3: Kiểm soát & Ingest Zero-Trust"
        p3s = "Propose-Not-Activate · Kiểm chứng @join · DriftMonitor (desc_hash) · Auto RollbackWatcher"
        l21 = "Truy vấn đã chọn"
        l31 = "Đổi ArcSwap (nguyên tử)"
    else:
        title = "GAP Three-Plane Strict Architectural Separation"
        p2 = "Plane 2: Progressive Intent Discovery"
        p2s = "L0/L1/L2 · Visibility Bitset (INV-05) · Baseline 95.24% / Optional S1 (INV-17)"
        p1 = "Plane 1: Zero-ML Deterministic Execution (ARC-002)"
        p1s = "AuthN + Cedar Policy -> PlanCache (TenantId) -> ArcSwap Snapshot -> BatchJoin -> EgressGuard"
        p3 = "Plane 3: Zero-Trust Control & Ingestion"
        p3s = "Propose-Not-Activate · Sampled @join Gates · DriftMonitor (desc_hash) · Auto Rollback"
        l21 = "Selected query"
        l31 = "Atomic ArcSwap publish"

    return f"""<figure class="diagram-frame">
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 860 324" width="860" height="324" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <defs>
      <marker id="tp-arr-accent" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--accent)"/>
      </marker>
      <marker id="tp-arr-warn" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--warn)"/>
      </marker>
    </defs>
    <rect x="14" y="10" width="832" height="76" rx="10" fill="var(--surface-alt)" stroke="var(--accent)" stroke-width="1.5"/>
    <text x="34" y="39" fill="var(--accent)" font-size="14" font-weight="700">{esc(p2)}</text>
    <text x="34" y="64" fill="var(--text)" font-size="12.5">{esc(p2s)}</text>

    <line x1="250" y1="86" x2="250" y2="122" stroke="var(--accent)" stroke-width="2" marker-end="url(#tp-arr-accent)"/>
    <rect x="266" y="92" width="210" height="24" rx="12" fill="var(--surface)" stroke="var(--accent)"/>
    <text x="371" y="108" text-anchor="middle" fill="var(--accent)" font-size="11.5" font-weight="600">{esc(l21)}</text>

    <rect x="14" y="124" width="832" height="78" rx="10" fill="var(--surface-alt)" stroke="var(--ok)" stroke-width="2"/>
    <text x="34" y="153" fill="var(--ok)" font-size="14" font-weight="700">{esc(p1)}</text>
    <text x="34" y="179" fill="var(--text)" font-size="12.5">{esc(p1s)}</text>

    <line x1="250" y1="238" x2="250" y2="204" stroke="var(--warn)" stroke-width="2" marker-end="url(#tp-arr-warn)"/>
    <rect x="266" y="209" width="230" height="24" rx="12" fill="var(--surface)" stroke="var(--warn)"/>
    <text x="381" y="225" text-anchor="middle" fill="var(--warn)" font-size="11.5" font-weight="600">{esc(l31)}</text>

    <rect x="14" y="240" width="832" height="74" rx="10" fill="var(--surface-alt)" stroke="var(--warn)" stroke-width="1.5"/>
    <text x="34" y="268" fill="var(--warn)" font-size="14" font-weight="700">{esc(p3)}</text>
    <text x="34" y="294" fill="var(--text)" font-size="12.5">{esc(p3s)}</text>
  </svg>
  <figcaption class="diagram-caption">{esc(title)}</figcaption>
</figure>"""


def svg_progressive_levels(lang: str) -> str:
    """Visual 2-row flow of L0 -> L1 (Cedar Bitset) -> L2 -> Execution holding 651 tools/list tokens."""
    if lang == "vi":
        title = "Quy trình Khám phá Lũy tiến 3 tầng (giữ tools/list cố định ở 651 tokens)"
        s0, s0b = "0. tools/list tĩnh", "5 tool cố định (651 tok)"
        s1, s1b = "1. L0: gap_describe", "overview: true (<=24 tok/nhánh)"
        s2, s2b = "2. L1: gap_search", "BM25 + Cedar Bitset (INV-05)"
        s3, s3b = "3. L2: gap_describe", "types: [\"shop_CustomersResult\"]"
        s4, s4b = "4. gap_query / gap_mutate", "Thực thi GraphQL định kiểu & batch"
    else:
        title = "Progressive Discovery L0 to L2 Flow (Fixed 651-Token tools/list)"
        s0, s0b = "0. Static tools/list", "5 static tools (651 tok)"
        s1, s1b = "1. L0: gap_describe", "overview: true (<=24 tok/branch)"
        s2, s2b = "2. L1: gap_search", "BM25 + Cedar Bitset (INV-05)"
        s3, s3b = "3. L2: gap_describe", "types: [\"shop_CustomersResult\"]"
        s4, s4b = "4. gap_query / gap_mutate", "Typed batched GraphQL execution"

    return f"""<figure class="diagram-frame">
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 860 236" width="860" height="236" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <defs>
      <marker id="pl-arr" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--accent)"/>
      </marker>
    </defs>
    <!-- Row 1: steps 0 -> 1 -> 2 -->
    <rect x="14" y="14" width="256" height="82" rx="10" fill="var(--surface-alt)" stroke="var(--accent)" stroke-width="1.5"/>
    <text x="142" y="46" text-anchor="middle" fill="var(--accent)" font-size="13" font-weight="700">{esc(s0)}</text>
    <text x="142" y="72" text-anchor="middle" fill="var(--muted)" font-size="11.5">{esc(s0b)}</text>
    <line x1="270" y1="55" x2="300" y2="55" stroke="var(--accent)" stroke-width="2" marker-end="url(#pl-arr)"/>

    <rect x="302" y="14" width="256" height="82" rx="10" fill="var(--surface-alt)" stroke="var(--border)" stroke-width="1.5"/>
    <text x="430" y="46" text-anchor="middle" fill="var(--text)" font-size="13" font-weight="700">{esc(s1)}</text>
    <text x="430" y="72" text-anchor="middle" fill="var(--muted)" font-size="11.5">{esc(s1b)}</text>
    <line x1="558" y1="55" x2="588" y2="55" stroke="var(--accent)" stroke-width="2" marker-end="url(#pl-arr)"/>

    <rect x="590" y="14" width="256" height="82" rx="10" fill="var(--surface-alt)" stroke="var(--warn)" stroke-width="1.5"/>
    <text x="718" y="46" text-anchor="middle" fill="var(--warn)" font-size="13" font-weight="700">{esc(s2)}</text>
    <text x="718" y="72" text-anchor="middle" fill="var(--muted)" font-size="11.5">{esc(s2b)}</text>

    <!-- Connector from step 2 down to step 3 -->
    <path d="M 718 96 L 718 117 L 212 117 L 212 138" fill="none" stroke="var(--accent)" stroke-width="2" marker-end="url(#pl-arr)"/>

    <!-- Row 2: steps 3 -> 4 -->
    <rect x="14" y="140" width="396" height="82" rx="10" fill="var(--surface-alt)" stroke="var(--border)" stroke-width="1.5"/>
    <text x="212" y="172" text-anchor="middle" fill="var(--text)" font-size="13" font-weight="700">{esc(s3)}</text>
    <text x="212" y="198" text-anchor="middle" fill="var(--muted)" font-size="12">{esc(s3b)}</text>
    <line x1="410" y1="181" x2="448" y2="181" stroke="var(--accent)" stroke-width="2" marker-end="url(#pl-arr)"/>

    <rect x="450" y="140" width="396" height="82" rx="10" fill="var(--surface-alt)" stroke="var(--ok)" stroke-width="2"/>
    <text x="648" y="172" text-anchor="middle" fill="var(--ok)" font-size="13" font-weight="700">{esc(s4)}</text>
    <text x="648" y="198" text-anchor="middle" fill="var(--muted)" font-size="12">{esc(s4b)}</text>
  </svg>
  <figcaption class="diagram-caption">{esc(title)}</figcaption>
</figure>"""


def svg_batch_join(lang: str) -> str:
    """Side-by-side comparison of N+1 LLM waterfall vs DataLoader BatchJoin (INV-18)."""
    if lang == "vi":
        title = "So sánh thác nước N+1 qua LLM và gom lô DataLoader BatchJoin INV-18"
        left_h = "Direct-MCP: Vòng lặp N+1 (4 lượt · 814.959 tok)"
        left_s1 = "LLM gọi customers -> nhận 50 ID"
        left_s2 = "LLM tự phát 50 HTTP rời: /orders?c=1 .. 50"
        right_h = "GAP @join: BatchJoin (2 lượt · 8.547 tok)"
        right_s1 = "gap-exec gom khóa duy nhất {1..50}"
        right_s2 = "1 khung BatchQuery ceil(K/max_batch) (INV-18)"
    else:
        title = "LLM N+1 Waterfall vs GAP DataLoader BatchJoin (INV-18)"
        left_h = "Direct-MCP: N+1 Loop (4 turns · 814,959 tok)"
        left_s1 = "LLM calls customers -> receives 50 IDs"
        left_s2 = "Agent fires 50 sequential HTTP calls"
        right_h = "GAP @join: BatchJoin (2 turns · 8,547 tok)"
        right_s1 = "gap-exec deduplicates keys {1..50}"
        right_s2 = "ceil(unique_keys / max_batch) BatchQuery (INV-18)"

    return f"""<figure class="diagram-frame">
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 860 198" width="860" height="198" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <defs>
      <marker id="bj-arr-warn" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--warn)"/>
      </marker>
      <marker id="bj-arr-ok" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--ok)"/>
      </marker>
    </defs>
    <rect x="12" y="10" width="406" height="178" rx="10" fill="var(--surface-alt)" stroke="var(--warn)" stroke-width="1.5"/>
    <text x="215" y="37" text-anchor="middle" fill="var(--warn)" font-size="13" font-weight="700">{esc(left_h)}</text>
    <rect x="28" y="54" width="374" height="40" rx="6" fill="var(--surface)" stroke="var(--border)"/>
    <text x="215" y="79" text-anchor="middle" fill="var(--text)" font-size="12">{esc(left_s1)}</text>
    <line x1="215" y1="94" x2="215" y2="128" stroke="var(--warn)" stroke-width="2" stroke-dasharray="4 3" marker-end="url(#bj-arr-warn)"/>
    <rect x="28" y="130" width="374" height="42" rx="6" fill="var(--surface)" stroke="var(--warn)"/>
    <text x="215" y="156" text-anchor="middle" fill="var(--warn)" font-size="12" font-weight="600">{esc(left_s2)}</text>

    <rect x="442" y="10" width="406" height="178" rx="10" fill="var(--surface-alt)" stroke="var(--ok)" stroke-width="2"/>
    <text x="645" y="37" text-anchor="middle" fill="var(--ok)" font-size="13" font-weight="700">{esc(right_h)}</text>
    <rect x="458" y="54" width="374" height="40" rx="6" fill="var(--surface)" stroke="var(--border)"/>
    <text x="645" y="79" text-anchor="middle" fill="var(--text)" font-size="12">{esc(right_s1)}</text>
    <line x1="645" y1="94" x2="645" y2="128" stroke="var(--ok)" stroke-width="2" marker-end="url(#bj-arr-ok)"/>
    <rect x="458" y="130" width="374" height="42" rx="6" fill="var(--surface)" stroke="var(--ok)"/>
    <text x="645" y="156" text-anchor="middle" fill="var(--ok)" font-size="12" font-weight="700">{esc(right_s2)}</text>
  </svg>
  <figcaption class="diagram-caption">{esc(title)}</figcaption>
</figure>"""


def svg_facade_flow(lang: str) -> str:
    """Three-axis host integration (MCP, A2A, AG-UI) + HTTP/2 & HTTP/3 flow."""
    if lang == "vi":
        title = "Luồng kết nối AI Host qua MCP 2026-07-28, A2A v1.0, AG-UI v1.0 và NGC"
        hosts = "AI Hosts & UI"
        hosts_l1 = "Claude · Cursor"
        hosts_l2 = "Antigravity"
        hosts_l3 = "Web AG-UI"
        trans = "HTTP/2 + HTTP/3 QUIC"
        trans_l1 = "TLS 1.3 (tắt 0-RTT)"
        gw1, gw2 = "Cổng GAP", "(gap-server)"
        gw_l1 = "Cedar · ArcSwap"
        gw_l2 = "DataLoader · BLAKE3"
        ngc = "6 NGC Connectors"
    else:
        title = "AI Host Integration via MCP 2026-07-28, A2A v1.0, AG-UI v1.0, and NGC"
        hosts = "AI Hosts & UI"
        hosts_l1 = "Claude · Cursor"
        hosts_l2 = "Antigravity"
        hosts_l3 = "AG-UI Clients"
        trans = "HTTP/2 + HTTP/3 QUIC"
        trans_l1 = "TLS 1.3 (0-RTT off)"
        gw1, gw2 = "GAP Gateway", "(gap-server)"
        gw_l1 = "Cedar · ArcSwap"
        gw_l2 = "DataLoader · BLAKE3"
        ngc = "6 NGC Connectors"

    return f"""<figure class="diagram-frame">
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 860 206" width="860" height="206" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <defs>
      <marker id="ff-arr-accent" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--accent)"/>
      </marker>
      <marker id="ff-arr-ok" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--ok)"/>
      </marker>
    </defs>
    <rect x="12" y="20" width="192" height="162" rx="10" fill="var(--surface-alt)" stroke="var(--border)"/>
    <text x="108" y="54" text-anchor="middle" fill="var(--text)" font-size="13.5" font-weight="700">{esc(hosts)}</text>
    <text x="108" y="90" text-anchor="middle" fill="var(--muted)" font-size="12">{esc(hosts_l1)}</text>
    <text x="108" y="114" text-anchor="middle" fill="var(--muted)" font-size="12">{esc(hosts_l2)}</text>
    <text x="108" y="138" text-anchor="middle" fill="var(--accent)" font-size="12" font-weight="600">{esc(hosts_l3)}</text>

    <line x1="204" y1="101" x2="224" y2="101" stroke="var(--accent)" stroke-width="2" marker-end="url(#ff-arr-accent)"/>
    <rect x="226" y="20" width="192" height="162" rx="10" fill="var(--surface-alt)" stroke="var(--accent)"/>
    <text x="322" y="54" text-anchor="middle" fill="var(--accent)" font-size="13.5" font-weight="700">{esc(trans)}</text>
    <text x="322" y="90" text-anchor="middle" fill="var(--text)" font-size="12">{esc(trans_l1)}</text>
    <text x="322" y="114" text-anchor="middle" fill="var(--muted)" font-size="12">/graphql · /mcp</text>
    <text x="322" y="138" text-anchor="middle" fill="var(--muted)" font-size="12">/ag-ui · A2A v1.0</text>

    <line x1="418" y1="101" x2="438" y2="101" stroke="var(--ok)" stroke-width="2" marker-end="url(#ff-arr-ok)"/>
    <rect x="440" y="20" width="192" height="162" rx="10" fill="var(--surface-alt)" stroke="var(--ok)" stroke-width="2"/>
    <text x="536" y="48" text-anchor="middle" fill="var(--ok)" font-size="13.5" font-weight="700">{esc(gw1)}</text>
    <text x="536" y="66" text-anchor="middle" fill="var(--ok)" font-size="11.5" font-weight="600">{esc(gw2)}</text>
    <text x="536" y="98" text-anchor="middle" fill="var(--text)" font-size="12">{esc(gw_l1)}</text>
    <text x="536" y="122" text-anchor="middle" fill="var(--text)" font-size="12">{esc(gw_l2)}</text>
    <text x="536" y="148" text-anchor="middle" fill="var(--muted)" font-size="11.5">5,013 req/s H3 · 28.86ms</text>

    <line x1="632" y1="101" x2="654" y2="101" stroke="var(--ok)" stroke-width="2" marker-end="url(#ff-arr-ok)"/>
    <rect x="656" y="20" width="192" height="162" rx="10" fill="var(--surface-alt)" stroke="var(--border)"/>
    <text x="752" y="54" text-anchor="middle" fill="var(--text)" font-size="13.5" font-weight="700">{esc(ngc)}</text>
    <text x="752" y="90" text-anchor="middle" fill="var(--muted)" font-size="12">OpenAPI · SQL</text>
    <text x="752" y="114" text-anchor="middle" fill="var(--muted)" font-size="12">GraphQL · gRPC</text>
    <text x="752" y="138" text-anchor="middle" fill="var(--muted)" font-size="12">CLI · Code AST</text>
  </svg>
  <figcaption class="diagram-caption">{esc(title)}</figcaption>
</figure>"""


def svg_query_mirror(lang: str) -> str:
    """Visual diagram showing a GraphQL/GAP-QL query mirroring the exact returned JSON shape 1:1."""
    if lang == "vi":
        title = "Truy vấn GAP-QL phản chiếu đúng 1:1 cấu trúc JSON nhận về"
        left_h = "1. Truy vấn bạn viết (GAP-QL)"
        mid_l = "Thực thi song song"
        right_h = "2. Kết quả JSON đúng hình dáng 1:1"
    else:
        title = "A GAP-QL Query Mirrors Its Returned JSON Shape 1:1"
        left_h = "1. Query Shape You Ask For"
        mid_l = "Typed Execution"
        right_h = "2. Exact 1:1 JSON Returned"

    return f"""<figure class="diagram-frame">
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 860 220" width="860" height="220" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <defs>
      <marker id="qm-arr" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--accent)"/>
      </marker>
    </defs>
    <rect x="14" y="14" width="338" height="192" rx="10" fill="var(--surface-alt)" stroke="var(--accent)" stroke-width="1.5"/>
    <text x="30" y="40" fill="var(--accent)" font-size="13" font-weight="700">{esc(left_h)}</text>
    <text x="30" y="68" fill="var(--text)" font-size="11.5" font-family="ui-monospace,monospace">query {{</text>
    <text x="44" y="90" fill="var(--text)" font-size="11.5" font-family="ui-monospace,monospace">customer(id: 1) {{</text>
    <text x="58" y="112" fill="var(--accent)" font-size="11.5" font-weight="700" font-family="ui-monospace,monospace">id   name   tier</text>
    <text x="44" y="134" fill="var(--text)" font-size="11.5" font-family="ui-monospace,monospace">}}</text>
    <text x="44" y="156" fill="var(--ok)" font-size="11.5" font-weight="700" font-family="ui-monospace,monospace">customer_notes(customer_id: 1)</text>
    <text x="30" y="178" fill="var(--text)" font-size="11.5" font-family="ui-monospace,monospace">}}</text>

    <line x1="354" y1="110" x2="504" y2="110" stroke="var(--accent)" stroke-width="2" marker-end="url(#qm-arr)"/>
    <rect x="358" y="76" width="144" height="26" rx="13" fill="var(--surface)" stroke="var(--accent)"/>
    <text x="430" y="93" text-anchor="middle" fill="var(--accent)" font-size="11" font-weight="700">{esc(mid_l)}</text>
    <text x="430" y="132" text-anchor="middle" fill="var(--muted)" font-size="11">1 HTTP Request</text>

    <rect x="508" y="14" width="338" height="192" rx="10" fill="var(--surface-alt)" stroke="var(--ok)" stroke-width="1.5"/>
    <text x="524" y="40" fill="var(--ok)" font-size="13" font-weight="700">{esc(right_h)}</text>
    <text x="524" y="68" fill="var(--text)" font-size="11.5" font-family="ui-monospace,monospace">{{ "data": {{</text>
    <text x="538" y="90" fill="var(--text)" font-size="11.5" font-family="ui-monospace,monospace">"customer": {{</text>
    <text x="552" y="112" fill="var(--accent)" font-size="11.5" font-weight="700" font-family="ui-monospace,monospace">"id": 1, "name": "Ada", "tier": "vip"</text>
    <text x="538" y="134" fill="var(--text)" font-size="11.5" font-family="ui-monospace,monospace">}},</text>
    <text x="538" y="156" fill="var(--ok)" font-size="11.5" font-weight="700" font-family="ui-monospace,monospace">"customer_notes": {{ "notes": [...] }}</text>
    <text x="524" y="178" fill="var(--text)" font-size="11.5" font-family="ui-monospace,monospace">}} }}</text>
  </svg>
  <figcaption class="diagram-caption">{esc(title)}</figcaption>
</figure>"""
