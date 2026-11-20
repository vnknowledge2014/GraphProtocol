"""Accessible localized inline SVG architecture diagrams using CSS theme variables."""
from __future__ import annotations

from .render import esc


def svg_mental_model(lang: str) -> str:
    """Before Direct-MCP vs Unified GAP Supergraph comparison diagram."""
    if lang == "vi":
        title = "So sánh Direct-MCP phẳng và Cổng GAP hợp nhất"
        b_head = "Cách cũ (Direct-MCP phẳng)"
        b_box1 = "Prompt 203.421 tokens"
        b_box2 = "1.000 JSON tool tải sẵn"
        b_arr = "Vòng lặp N+1 qua LLM (4 lượt)"
        b_box3 = "Mô tả tự sửa (Rug-Pull) + 0 Cedar"
        g_head = "Với Cổng GAP (Supergraph định kiểu)"
        g_box1 = "5 MCP Tools tĩnh (651 tok)"
        g_box2 = "Khám phá L0 -> L1 -> L2 + Cedar"
        g_arr = "DataLoader Batch + ArcSwap (2 lượt)"
        g_box3 = "6 NGC Adapters · Chặn SSRF & Drift"
    else:
        title = "Comparison of flat Direct-MCP vs Unified GAP Supergraph"
        b_head = "Without GAP (Flat Direct-MCP)"
        b_box1 = "203,421 Token Prompt Bloat"
        b_box2 = "1,000 raw tool schemas up front"
        b_arr = "Agent-orchestrated N+1 loop (4 turns)"
        b_box3 = "Mutable descriptions (Rug-Pull risk)"
        g_head = "With GAP (Typed Supergraph)"
        g_box1 = "5 Static MCP Tools (651 tok)"
        g_box2 = "L0 -> L1 -> L2 Discovery + Cedar AuthZ"
        g_arr = "DataLoader Batching + ArcSwap (2 turns)"
        g_box3 = "6 NGC Branches · SSRF & Drift Guard"

    return f"""<figure class="diagram-frame">
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 760 270" width="760" height="270" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <defs>
      <marker id="mm-arr-warn" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--warn)"/>
      </marker>
      <marker id="mm-arr-ok" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--ok)"/>
      </marker>
    </defs>
    <rect x="8" y="8" width="356" height="254" rx="12" fill="var(--surface-alt)" stroke="var(--warn)" stroke-width="1.5"/>
    <text x="26" y="36" fill="var(--warn)" font-size="14" font-weight="700">{esc(b_head)}</text>
    <rect x="26" y="54" width="320" height="52" rx="8" fill="var(--surface)" stroke="var(--border)"/>
    <text x="186" y="77" text-anchor="middle" fill="var(--text)" font-size="13" font-weight="700">{esc(b_box1)}</text>
    <text x="186" y="96" text-anchor="middle" fill="var(--muted)" font-size="12">{esc(b_box2)}</text>
    <line x1="186" y1="106" x2="186" y2="146" stroke="var(--warn)" stroke-width="2" stroke-dasharray="4 3" marker-end="url(#mm-arr-warn)"/>
    <text x="196" y="131" text-anchor="start" fill="var(--warn)" font-size="11.5" font-weight="600">{esc(b_arr)}</text>
    <rect x="26" y="150" width="320" height="90" rx="8" fill="var(--surface)" stroke="var(--border)"/>
    <text x="186" y="185" text-anchor="middle" fill="var(--text)" font-size="13" font-weight="600">{esc(b_box3)}</text>
    <text x="186" y="214" text-anchor="middle" fill="var(--muted)" font-size="12">REST  |  SQL  |  gRPC  |  MCP  |  Code AST</text>

    <rect x="396" y="8" width="356" height="254" rx="12" fill="var(--surface-alt)" stroke="var(--accent)" stroke-width="2"/>
    <text x="414" y="36" fill="var(--accent)" font-size="14" font-weight="700">{esc(g_head)}</text>
    <rect x="414" y="54" width="320" height="52" rx="8" fill="var(--surface)" stroke="var(--accent)"/>
    <text x="574" y="77" text-anchor="middle" fill="var(--accent)" font-size="13" font-weight="700">{esc(g_box1)}</text>
    <text x="574" y="96" text-anchor="middle" fill="var(--muted)" font-size="12">{esc(g_box2)}</text>
    <line x1="574" y1="106" x2="574" y2="146" stroke="var(--ok)" stroke-width="2" marker-end="url(#mm-arr-ok)"/>
    <text x="584" y="131" text-anchor="start" fill="var(--ok)" font-size="11.5" font-weight="600">{esc(g_arr)}</text>
    <rect x="414" y="150" width="320" height="90" rx="8" fill="var(--surface)" stroke="var(--ok)"/>
    <text x="574" y="185" text-anchor="middle" fill="var(--ok)" font-size="13" font-weight="700">{esc(g_box3)}</text>
    <text x="574" y="214" text-anchor="middle" fill="var(--muted)" font-size="12">HTTP/3 QUIC  ·  MCP  ·  A2A v1.0  ·  AG-UI</text>
  </svg>
</figure>"""


def svg_three_planes(lang: str) -> str:
    """Three-Plane separation diagram (Execution Zero-ML, Discovery, Control)."""
    if lang == "vi":
        title = "Kiến trúc 3 Mặt phẳng tách biệt hoàn toàn trong GAP"
        p2 = "Mặt phẳng 2: Khám phá Lũy tiến"
        p2s = "L0/L1/L2 · Visibility Bitset (INV-05) · Baseline / S1 tùy chọn (INV-17)"
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
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 760 296" width="760" height="296" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <defs>
      <marker id="tp-arr-accent" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--accent)"/>
      </marker>
      <marker id="tp-arr-warn" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--warn)"/>
      </marker>
    </defs>
    <rect x="14" y="10" width="732" height="72" rx="10" fill="var(--surface-alt)" stroke="var(--accent)" stroke-width="1.5"/>
    <text x="32" y="38" fill="var(--accent)" font-size="14" font-weight="700">{esc(p2)}</text>
    <text x="32" y="62" fill="var(--text)" font-size="12.5">{esc(p2s)}</text>

    <line x1="380" y1="82" x2="380" y2="108" stroke="var(--accent)" stroke-width="2" marker-end="url(#tp-arr-accent)"/>
    <text x="392" y="99" fill="var(--accent)" font-size="11.5" font-weight="600">{esc(l21)}</text>

    <rect x="14" y="110" width="732" height="76" rx="10" fill="var(--surface-alt)" stroke="var(--ok)" stroke-width="2"/>
    <text x="32" y="139" fill="var(--ok)" font-size="14" font-weight="700">{esc(p1)}</text>
    <text x="32" y="164" fill="var(--text)" font-size="12.5">{esc(p1s)}</text>

    <line x1="380" y1="214" x2="380" y2="188" stroke="var(--warn)" stroke-width="2" marker-end="url(#tp-arr-warn)"/>
    <text x="392" y="205" fill="var(--warn)" font-size="11.5" font-weight="600">{esc(l31)}</text>

    <rect x="14" y="214" width="732" height="72" rx="10" fill="var(--surface-alt)" stroke="var(--warn)" stroke-width="1.5"/>
    <text x="32" y="242" fill="var(--warn)" font-size="14" font-weight="700">{esc(p3)}</text>
    <text x="32" y="266" fill="var(--text)" font-size="12.5">{esc(p3s)}</text>
  </svg>
</figure>"""


def svg_progressive_levels(lang: str) -> str:
    """Visual flow of L0 -> L1 (Cedar Bitset) -> L2 -> Execution holding 651 tools/list tokens."""
    if lang == "vi":
        title = "Quy trình Khám phá Lũy tiến 3 tầng (651 tokens) so với tải phẳng"
        s0, s0b = "tools/list tĩnh", "5 tool (651 tok)"
        s1, s1b = "L0: gap_describe", "overview: true (<=24 tok/nhánh)"
        s2, s2b = "L1: gap_search", "BM25 + Cedar Bitset (INV-05)"
        s3, s3b = "L2: gap_describe", "types: [\"shop_CustomersResult\"]"
        s4, s4b = "gap_query / mutate", "Thực thi GraphQL có batch"
    else:
        title = "Progressive Discovery L0 to L2 Flow (Fixed 651-Token tools/list)"
        s0, s0b = "Static tools/list", "5 tools (651 tok)"
        s1, s1b = "L0: gap_describe", "overview: true (<=24 tok/branch)"
        s2, s2b = "L1: gap_search", "BM25 + Cedar Bitset (INV-05)"
        s3, s3b = "L2: gap_describe", "types: [\"shop_CustomersResult\"]"
        s4, s4b = "gap_query / mutate", "Typed batched execution"

    return f"""<figure class="diagram-frame">
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 760 130" width="760" height="130" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <defs>
      <marker id="pl-arr" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--accent)"/>
      </marker>
    </defs>
    <rect x="8" y="22" width="128" height="82" rx="8" fill="var(--surface-alt)" stroke="var(--accent)" stroke-width="1.5"/>
    <text x="72" y="54" text-anchor="middle" fill="var(--accent)" font-size="12" font-weight="700">{esc(s0)}</text>
    <text x="72" y="78" text-anchor="middle" fill="var(--muted)" font-size="10.5">{esc(s0b)}</text>
    <line x1="136" y1="63" x2="156" y2="63" stroke="var(--accent)" stroke-width="2" marker-end="url(#pl-arr)"/>

    <rect x="158" y="22" width="134" height="82" rx="8" fill="var(--surface-alt)" stroke="var(--border)"/>
    <text x="225" y="54" text-anchor="middle" fill="var(--text)" font-size="12" font-weight="700">{esc(s1)}</text>
    <text x="225" y="78" text-anchor="middle" fill="var(--muted)" font-size="10">{esc(s1b)}</text>
    <line x1="292" y1="63" x2="312" y2="63" stroke="var(--accent)" stroke-width="2" marker-end="url(#pl-arr)"/>

    <rect x="314" y="22" width="136" height="82" rx="8" fill="var(--surface-alt)" stroke="var(--warn)" stroke-width="1.5"/>
    <text x="382" y="54" text-anchor="middle" fill="var(--warn)" font-size="12" font-weight="700">{esc(s2)}</text>
    <text x="382" y="78" text-anchor="middle" fill="var(--muted)" font-size="10">{esc(s2b)}</text>
    <line x1="450" y1="63" x2="470" y2="63" stroke="var(--accent)" stroke-width="2" marker-end="url(#pl-arr)"/>

    <rect x="472" y="22" width="134" height="82" rx="8" fill="var(--surface-alt)" stroke="var(--border)"/>
    <text x="539" y="54" text-anchor="middle" fill="var(--text)" font-size="12" font-weight="700">{esc(s3)}</text>
    <text x="539" y="78" text-anchor="middle" fill="var(--muted)" font-size="10">{esc(s3b)}</text>
    <line x1="606" y1="63" x2="626" y2="63" stroke="var(--accent)" stroke-width="2" marker-end="url(#pl-arr)"/>

    <rect x="628" y="22" width="124" height="82" rx="8" fill="var(--surface-alt)" stroke="var(--ok)" stroke-width="2"/>
    <text x="690" y="54" text-anchor="middle" fill="var(--ok)" font-size="12" font-weight="700">{esc(s4)}</text>
    <text x="690" y="78" text-anchor="middle" fill="var(--muted)" font-size="10">{esc(s4b)}</text>
  </svg>
</figure>"""


def svg_batch_join(lang: str) -> str:
    """Side-by-side comparison of N+1 LLM waterfall vs DataLoader BatchJoin (INV-18)."""
    if lang == "vi":
        title = "So sánh thác nước N+1 qua LLM và gom lô DataLoader BatchJoin INV-18"
        left_h = "Direct-MCP: Vòng lặp N+1 (4 lượt · 814.959 tok)"
        left_s1 = "LLM gọi customers -> nhận 50 ID"
        left_s2 = "LLM tự phát 50 HTTP rời: /orders?c=1 .. 50"
        right_h = "GAP @join: DataLoader BatchJoin (2 lượt · 8.547 tok)"
        right_s1 = "gap-plan gom khóa duy nhất {1..50}"
        right_s2 = "1 khung BatchQuery ceil(K/max_batch) (INV-18)"
    else:
        title = "LLM N+1 Waterfall vs GAP DataLoader BatchJoin (INV-18)"
        left_h = "Direct-MCP: N+1 Loop (4 turns · 814,959 tok)"
        left_s1 = "LLM calls customers -> receives 50 IDs"
        left_s2 = "Agent fires 50 sequential HTTP calls"
        right_h = "GAP @join: DataLoader Batch (2 turns · 8,547 tok)"
        right_s1 = "gap-exec deduplicates keys {1..50}"
        right_s2 = "ceil(unique_keys / max_batch) BatchQuery (INV-18)"

    return f"""<figure class="diagram-frame">
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 760 170" width="760" height="170" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <defs>
      <marker id="bj-arr-warn" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--warn)"/>
      </marker>
      <marker id="bj-arr-ok" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--ok)"/>
      </marker>
    </defs>
    <rect x="8" y="10" width="356" height="150" rx="10" fill="var(--surface-alt)" stroke="var(--warn)" stroke-width="1.5"/>
    <text x="24" y="36" fill="var(--warn)" font-size="13" font-weight="700">{esc(left_h)}</text>
    <rect x="24" y="50" width="324" height="36" rx="6" fill="var(--surface)" stroke="var(--border)"/>
    <text x="186" y="73" text-anchor="middle" fill="var(--text)" font-size="12">{esc(left_s1)}</text>
    <line x1="186" y1="86" x2="186" y2="108" stroke="var(--warn)" stroke-width="2" stroke-dasharray="4 3" marker-end="url(#bj-arr-warn)"/>
    <rect x="24" y="110" width="324" height="36" rx="6" fill="var(--surface)" stroke="var(--warn)"/>
    <text x="186" y="133" text-anchor="middle" fill="var(--warn)" font-size="12" font-weight="600">{esc(left_s2)}</text>

    <rect x="396" y="10" width="356" height="150" rx="10" fill="var(--surface-alt)" stroke="var(--ok)" stroke-width="2"/>
    <text x="412" y="36" fill="var(--ok)" font-size="13" font-weight="700">{esc(right_h)}</text>
    <rect x="412" y="50" width="324" height="36" rx="6" fill="var(--surface)" stroke="var(--border)"/>
    <text x="574" y="73" text-anchor="middle" fill="var(--text)" font-size="12">{esc(right_s1)}</text>
    <line x1="574" y1="86" x2="574" y2="108" stroke="var(--ok)" stroke-width="2" marker-end="url(#bj-arr-ok)"/>
    <rect x="412" y="110" width="324" height="36" rx="6" fill="var(--surface)" stroke="var(--ok)"/>
    <text x="574" y="133" text-anchor="middle" fill="var(--ok)" font-size="12" font-weight="700">{esc(right_s2)}</text>
  </svg>
</figure>"""


def svg_facade_flow(lang: str) -> str:
    """Three-axis host integration (MCP, A2A, AG-UI) + HTTP/2 & HTTP/3 flow."""
    if lang == "vi":
        title = "Luồng kết nối AI Host qua MCP 2026-07-28, A2A v1.0, AG-UI v1.0 và NGC"
        hosts = "AI Hosts & UI"
        hosts_s = "Claude · Cursor · Antigravity · Web AG-UI"
        trans = "HTTP/2 + HTTP/3 QUIC"
        trans_s = "TLS 1.3 (tắt 0-RTT) · /graphql · /mcp · /ag-ui · A2A v1.0"
        gw = "Cổng GAP (gap-server)"
        gw_s = "Cedar · ArcSwap · DataLoader · BLAKE3"
        ngc = "6 NGC Connectors"
    else:
        title = "AI Host Integration via MCP 2026-07-28, A2A v1.0, AG-UI v1.0, and NGC"
        hosts = "AI Hosts & UI"
        hosts_s = "Claude · Cursor · Antigravity · AG-UI Clients"
        trans = "HTTP/2 + HTTP/3 QUIC"
        trans_s = "TLS 1.3 (0-RTT off) · /graphql · /mcp · /ag-ui · Agent Card"
        gw = "GAP Gateway (gap-server)"
        gw_s = "Cedar · ArcSwap · DataLoader · BLAKE3"
        ngc = "6 NGC Connectors"

    return f"""<figure class="diagram-frame">
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 760 190" width="760" height="190" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <defs>
      <marker id="ff-arr-accent" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--accent)"/>
      </marker>
      <marker id="ff-arr-ok" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
        <path d="M 0 1.5 L 9 5 L 0 8.5 z" fill="var(--ok)"/>
      </marker>
    </defs>
    <rect x="10" y="28" width="165" height="130" rx="10" fill="var(--surface-alt)" stroke="var(--border)"/>
    <text x="92" y="64" text-anchor="middle" fill="var(--text)" font-size="13.5" font-weight="700">{esc(hosts)}</text>
    <text x="92" y="92" text-anchor="middle" fill="var(--muted)" font-size="11.5">{esc(hosts_s.split('·')[0].strip())}</text>
    <text x="92" y="112" text-anchor="middle" fill="var(--muted)" font-size="11.5">{esc(' · '.join(hosts_s.split('·')[1:3]).strip())}</text>
    <text x="92" y="132" text-anchor="middle" fill="var(--accent)" font-size="11.5" font-weight="600">{esc(hosts_s.split('·')[-1].strip())}</text>

    <line x1="175" y1="93" x2="208" y2="93" stroke="var(--accent)" stroke-width="2" marker-end="url(#ff-arr-accent)"/>
    <rect x="210" y="28" width="165" height="130" rx="10" fill="var(--surface-alt)" stroke="var(--accent)"/>
    <text x="292" y="64" text-anchor="middle" fill="var(--accent)" font-size="13.5" font-weight="700">{esc(trans)}</text>
    <text x="292" y="92" text-anchor="middle" fill="var(--text)" font-size="11.5">{esc(trans_s.split('·')[0].strip())}</text>
    <text x="292" y="112" text-anchor="middle" fill="var(--muted)" font-size="11.5">/graphql · /mcp</text>
    <text x="292" y="132" text-anchor="middle" fill="var(--muted)" font-size="11.5">/ag-ui · A2A v1.0</text>

    <line x1="375" y1="93" x2="408" y2="93" stroke="var(--ok)" stroke-width="2" marker-end="url(#ff-arr-ok)"/>
    <rect x="410" y="28" width="165" height="130" rx="10" fill="var(--surface-alt)" stroke="var(--ok)" stroke-width="2"/>
    <text x="492" y="64" text-anchor="middle" fill="var(--ok)" font-size="13.5" font-weight="700">{esc(gw)}</text>
    <text x="492" y="94" text-anchor="middle" fill="var(--text)" font-size="11.5">{esc(' · '.join(gw_s.split('·')[:2]).strip())}</text>
    <text x="492" y="116" text-anchor="middle" fill="var(--text)" font-size="11.5">{esc(' · '.join(gw_s.split('·')[2:]).strip())}</text>
    <text x="492" y="138" text-anchor="middle" fill="var(--muted)" font-size="11.5">5,013 req/s H3 · 28.86ms</text>

    <line x1="575" y1="93" x2="603" y2="93" stroke="var(--ok)" stroke-width="2" marker-end="url(#ff-arr-ok)"/>
    <rect x="605" y="28" width="145" height="130" rx="10" fill="var(--surface-alt)" stroke="var(--border)"/>
    <text x="677" y="64" text-anchor="middle" fill="var(--text)" font-size="13.5" font-weight="700">{esc(ngc)}</text>
    <text x="677" y="94" text-anchor="middle" fill="var(--muted)" font-size="11.5">OpenAPI · SQL</text>
    <text x="677" y="114" text-anchor="middle" fill="var(--muted)" font-size="11.5">GraphQL · gRPC</text>
    <text x="677" y="134" text-anchor="middle" fill="var(--muted)" font-size="11.5">CLI · Code AST</text>
  </svg>
</figure>"""
