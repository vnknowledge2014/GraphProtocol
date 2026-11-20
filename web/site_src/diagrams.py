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
    <rect x="8" y="8" width="356" height="254" rx="12" fill="var(--surface-alt)" stroke="var(--warn)" stroke-width="1.5"/>
    <text x="26" y="36" fill="var(--warn)" font-size="14" font-weight="700">{esc(b_head)}</text>
    <rect x="26" y="54" width="320" height="52" rx="8" fill="var(--surface)" stroke="var(--border)"/>
    <text x="186" y="77" text-anchor="middle" fill="var(--text)" font-size="13" font-weight="700">{esc(b_box1)}</text>
    <text x="186" y="96" text-anchor="middle" fill="var(--muted)" font-size="12">{esc(b_box2)}</text>
    <line x1="186" y1="106" x2="186" y2="146" stroke="var(--warn)" stroke-width="2" stroke-dasharray="4 3"/>
    <text x="186" y="131" text-anchor="middle" fill="var(--warn)" font-size="12" font-weight="600">{esc(b_arr)}</text>
    <rect x="26" y="150" width="320" height="90" rx="8" fill="var(--surface)" stroke="var(--border)"/>
    <text x="186" y="185" text-anchor="middle" fill="var(--text)" font-size="13" font-weight="600">{esc(b_box3)}</text>
    <text x="186" y="214" text-anchor="middle" fill="var(--muted)" font-size="12">REST  |  SQL  |  gRPC  |  MCP  |  Code AST</text>

    <rect x="396" y="8" width="356" height="254" rx="12" fill="var(--surface-alt)" stroke="var(--accent)" stroke-width="2"/>
    <text x="414" y="36" fill="var(--accent)" font-size="14" font-weight="700">{esc(g_head)}</text>
    <rect x="414" y="54" width="320" height="52" rx="8" fill="var(--surface)" stroke="var(--accent)"/>
    <text x="574" y="77" text-anchor="middle" fill="var(--accent)" font-size="13" font-weight="700">{esc(g_box1)}</text>
    <text x="574" y="96" text-anchor="middle" fill="var(--muted)" font-size="12">{esc(g_box2)}</text>
    <line x1="574" y1="106" x2="574" y2="146" stroke="var(--ok)" stroke-width="2"/>
    <text x="574" y="131" text-anchor="middle" fill="var(--ok)" font-size="12" font-weight="600">{esc(g_arr)}</text>
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
    else:
        title = "GAP Three-Plane Strict Architectural Separation"
        p2 = "Plane 2: Progressive Intent Discovery"
        p2s = "L0/L1/L2 · Visibility Bitset (INV-05) · Baseline 95.24% / Optional S1 (INV-17)"
        p1 = "Plane 1: Zero-ML Deterministic Execution (ARC-002)"
        p1s = "AuthN + Cedar Policy -> PlanCache (TenantId) -> ArcSwap Snapshot -> BatchJoin -> EgressGuard"
        p3 = "Plane 3: Zero-Trust Control & Ingestion"
        p3s = "Propose-Not-Activate · Sampled @join Gates · DriftMonitor (desc_hash) · Auto Rollback"

    return f"""<figure class="diagram-frame">
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 760 278" width="760" height="278" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <rect x="14" y="14" width="732" height="72" rx="10" fill="var(--surface-alt)" stroke="var(--accent)" stroke-width="1.5"/>
    <text x="32" y="42" fill="var(--accent)" font-size="14" font-weight="700">{esc(p2)}</text>
    <text x="32" y="66" fill="var(--text)" font-size="12.5">{esc(p2s)}</text>

    <rect x="14" y="102" width="732" height="76" rx="10" fill="var(--surface-alt)" stroke="var(--ok)" stroke-width="2"/>
    <text x="32" y="131" fill="var(--ok)" font-size="14" font-weight="700">{esc(p1)}</text>
    <text x="32" y="156" fill="var(--text)" font-size="12.5">{esc(p1s)}</text>

    <rect x="14" y="194" width="732" height="72" rx="10" fill="var(--surface-alt)" stroke="var(--warn)" stroke-width="1.5"/>
    <text x="32" y="222" fill="var(--warn)" font-size="14" font-weight="700">{esc(p3)}</text>
    <text x="32" y="246" fill="var(--text)" font-size="12.5">{esc(p3s)}</text>
  </svg>
</figure>"""


def svg_facade_flow(lang: str) -> str:
    """Three-axis host integration (MCP, A2A, AG-UI) + HTTP/2 & HTTP/3 flow."""
    if lang == "vi":
        title = "Luồng kết nối AI Host qua MCP 2026-07-28, A2A v1.0, AG-UI v1.0 và NGC"
        hosts = "AI Hosts & UI"
        hosts_s = "Claude · Cursor · Antigravity · Web AG-UI"
        trans = "HTTP/2 + HTTP/3 QUIC"
        trans_s = "TLS 1.3 (tắt 0-RTT) · /graphql · /mcp · /ag-ui · /.well-known/agent-card.json"
        gw = "Cổng GAP (gap-server)"
        gw_s = "Cedar · ArcSwap · DataLoader · BLAKE3"
        ngc = "6 NGC Connectors"
        ngc_s = "OpenAPI · SQL · GraphQL · gRPC · CLI · Code"
    else:
        title = "AI Host Integration via MCP 2026-07-28, A2A v1.0, AG-UI v1.0, and NGC"
        hosts = "AI Hosts & UI"
        hosts_s = "Claude · Cursor · Antigravity · AG-UI Clients"
        trans = "HTTP/2 + HTTP/3 QUIC"
        trans_s = "TLS 1.3 (0-RTT off) · /graphql · /mcp · /ag-ui · Agent Card"
        gw = "GAP Gateway (gap-server)"
        gw_s = "Cedar · ArcSwap · DataLoader · BLAKE3"
        ngc = "6 NGC Connectors"
        ngc_s = "OpenAPI · SQL · GraphQL · gRPC · CLI · Code"

    return f"""<figure class="diagram-frame">
  <svg role="img" aria-label="{esc(title)}" viewBox="0 0 760 190" width="760" height="190" xmlns="http://www.w3.org/2000/svg">
    <title>{esc(title)}</title>
    <rect x="10" y="28" width="165" height="130" rx="10" fill="var(--surface-alt)" stroke="var(--border)"/>
    <text x="92" y="64" text-anchor="middle" fill="var(--text)" font-size="13.5" font-weight="700">{esc(hosts)}</text>
    <text x="92" y="92" text-anchor="middle" fill="var(--muted)" font-size="11.5">{esc(hosts_s.split('·')[0].strip())}</text>
    <text x="92" y="112" text-anchor="middle" fill="var(--muted)" font-size="11.5">{esc(' · '.join(hosts_s.split('·')[1:3]).strip())}</text>
    <text x="92" y="132" text-anchor="middle" fill="var(--accent)" font-size="11.5" font-weight="600">{esc(hosts_s.split('·')[-1].strip())}</text>

    <line x1="175" y1="93" x2="210" y2="93" stroke="var(--accent)" stroke-width="2"/>
    <rect x="210" y="28" width="165" height="130" rx="10" fill="var(--surface-alt)" stroke="var(--accent)"/>
    <text x="292" y="64" text-anchor="middle" fill="var(--accent)" font-size="13.5" font-weight="700">{esc(trans)}</text>
    <text x="292" y="92" text-anchor="middle" fill="var(--text)" font-size="11.5">{esc(trans_s.split('·')[0].strip())}</text>
    <text x="292" y="112" text-anchor="middle" fill="var(--muted)" font-size="11.5">/graphql · /mcp</text>
    <text x="292" y="132" text-anchor="middle" fill="var(--muted)" font-size="11.5">/ag-ui · A2A v1.0</text>

    <line x1="375" y1="93" x2="410" y2="93" stroke="var(--ok)" stroke-width="2"/>
    <rect x="410" y="28" width="165" height="130" rx="10" fill="var(--surface-alt)" stroke="var(--ok)" stroke-width="2"/>
    <text x="492" y="64" text-anchor="middle" fill="var(--ok)" font-size="13.5" font-weight="700">{esc(gw)}</text>
    <text x="492" y="94" text-anchor="middle" fill="var(--text)" font-size="11.5">{esc(' · '.join(gw_s.split('·')[:2]).strip())}</text>
    <text x="492" y="116" text-anchor="middle" fill="var(--text)" font-size="11.5">{esc(' · '.join(gw_s.split('·')[2:]).strip())}</text>
    <text x="492" y="138" text-anchor="middle" fill="var(--muted)" font-size="11.5">5,013 req/s H3 · 28.86ms</text>

    <line x1="575" y1="93" x2="605" y2="93" stroke="var(--border)" stroke-width="2"/>
    <rect x="605" y="28" width="145" height="130" rx="10" fill="var(--surface-alt)" stroke="var(--border)"/>
    <text x="677" y="64" text-anchor="middle" fill="var(--text)" font-size="13.5" font-weight="700">{esc(ngc)}</text>
    <text x="677" y="94" text-anchor="middle" fill="var(--muted)" font-size="11.5">OpenAPI · SQL</text>
    <text x="677" y="114" text-anchor="middle" fill="var(--muted)" font-size="11.5">GraphQL · gRPC</text>
    <text x="677" y="134" text-anchor="middle" fill="var(--muted)" font-size="11.5">CLI · Code AST</text>
  </svg>
</figure>"""
