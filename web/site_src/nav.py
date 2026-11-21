"""Site navigation tree, page registry, UI labels, breadcrumbs, and pagination helpers."""
from __future__ import annotations

import posixpath

SECTIONS = [
    ("start-here", "Start Here", "Bắt đầu"),
    ("learn", "Learn Basics", "Kiến thức Nền tảng"),
    ("academy", "GAP Academy (Courses)", "Học viện GAP (Thực chiến)"),
    ("concepts", "Core Concepts", "Kiến trúc Cốt lõi"),
    ("connectors", "Connect Data & Tools (NGC)", "Kết nối Dữ liệu (NGC)"),
    ("agents", "AI Agent Hosts & Routers", "Tích hợp AI Agent"),
    ("security", "Zero-Trust Governance", "Bảo mật Zero-Trust"),
    ("lab", "Interactive Lab", "Phòng Lab Thực hành"),
    ("reference", "Operations & Reference", "Vận hành & Tra cứu"),
]

# Each tuple defines one symmetric bilingual page:
# (slug, rel_path, section_id, title_en, title_vi, read_min, summary_en, summary_vi)
PAGES = [
    (
        "home",
        "index.html",
        "start-here",
        "GAP Documentation Hub",
        "Trang chủ Tài liệu GAP",
        3,
        "Unified typed GraphQL-profile gateway, progressive discovery, and zero-trust runtime governance for AI agents.",
        "Cổng gateway GraphQL định kiểu chặt chẽ, khám phá lũy tiến và quản trị zero-trust cho AI Agent.",
    ),
    (
        "what-is-gap",
        "getting-started/what-is-gap.html",
        "start-here",
        "What is GAP?",
        "GAP là gì?",
        5,
        "Understand why Direct-MCP hits prompt bloat and N+1 waterfalls, and how a unified GAP supergraph solves both.",
        "Hiểu vì sao Direct-MCP phình prompt và dính lỗi N+1, và cách một supergraph GAP thống nhất giải quyết cả hai.",
    ),
    (
        "quickstart",
        "getting-started/quickstart.html",
        "start-here",
        "10-Minute Quickstart",
        "Khởi chạy Nhanh (10 phút)",
        10,
        "Validate gap.toml, boot the Walking Skeleton REST + MCP supergraph, run a joined query, and attach an AI host.",
        "Kiểm tra gap.toml, bật Walking Skeleton REST + MCP, chạy truy vấn gộp đầu tiên và gắn vào AI host.",
    ),
    (
        "learn-graphql-and-gap",
        "learn/graphql-and-gap-basics.html",
        "learn",
        "GraphQL & GAP Basics",
        "Nhập môn GraphQL & GAP",
        8,
        "Zero-jargon foundations: unified supergraph analogy, types, scalars, nullability (!), and interactive 1:1 query-to-JSON explorer.",
        "Kiến thức nền dễ hiểu: ẩn dụ siêu đồ thị thống nhất, kiểu dữ liệu, scalar, quy tắc khác null (!) và bộ dựng truy vấn 1:1.",
    ),
    (
        "learn-queries-mutations-joins",
        "learn/queries-mutations-joins.html",
        "learn",
        "Arguments, Mutations & Joins",
        "Tham số, Mutation & Nối nhánh",
        8,
        "Filter with variables, run safe @sideEffect dry-run mutations, traverse cross-branch edges, and translate REST/MCP habits to GAP.",
        "Lọc dữ liệu với biến tham số, chạy mutation an toàn qua @sideEffect và đối chiếu tư duy cho kỹ sư REST & Direct-MCP.",
    ),
    (
        "academy-hub",
        "academy/index.html",
        "academy",
        "GAP Academy — Hands-On Courses",
        "Học viện GAP — Khóa học Thực chiến",
        5,
        "Structured Odyssey-style tracks combining theory, real CLI drills, fill-in-the-blank challenges, and persistent progress tracking.",
        "Lộ trình học tập thực chiến kiểu Odyssey kết hợp lý thuyết, bài tập điền khuyết lệnh thật và lưu tiến độ tự động.",
    ),
    (
        "course-liftoff",
        "academy/liftoff-first-supergraph.html",
        "academy",
        "Course 1: Lift-Off — First Supergraph",
        "Khóa 1: Lift-Off — Siêu đồ thị Đầu tiên",
        20,
        "4 hands-on lessons: scaffold with gap init, wire OpenAPI + MCP branches, verify /readyz, and run your first cross-branch query.",
        "4 bài thực hành: khởi tạo bằng gap init, nối OpenAPI + MCP, kiểm tra /readyz và gửi truy vấn gộp liên nhánh đầu tiên.",
    ),
    (
        "course-mcp-agents",
        "academy/mcp-and-ai-assistants.html",
        "academy",
        "Course 2: MCP, AG-UI & AI Assistants",
        "Khóa 2: MCP, AG-UI & Trợ lý AI",
        20,
        "4 lessons: defeat 203,421-token prompt bloat, master L0->L1->L2 discovery, wire Claude/Cursor --stdio, and stream AG-UI SSE.",
        "4 bài thực hành: thoát bẫy 203.421 token, làm chủ khám phá L0->L1->L2, nối Claude/Cursor --stdio và phát luồng SSE AG-UI.",
    ),
    (
        "course-connectors-batching",
        "academy/connectors-and-batching.html",
        "academy",
        "Course 3: REST, SQL & N+1 Batching",
        "Khóa 3: REST, SQL & Gom lô N+1",
        25,
        "4 lessons: diagnose N+1 waterfalls, batch with x-gap-in-param (INV-18), add SQL & Code AST branches, and approve proposals safely.",
        "4 bài thực hành: bắt bệnh thác nước N+1, gom lô x-gap-in-param (INV-18), thêm SQL & Code AST và duyệt snapshot an toàn.",
    ),
    (
        "course-enterprise-zero-trust",
        "academy/enterprise-zero-trust.html",
        "academy",
        "Course 4: Zero-Trust, Cedar & HTTP/3",
        "Khóa 4: Zero-Trust, Cedar & HTTP/3",
        25,
        "4 lessons: enforce T0/T1/T2 + <gap:untrusted>, write Cedar two-person rules, block rug pulls & SSRF, and deploy HTTP/3 QUIC.",
        "4 bài thực hành: phân tầng T0/T1/T2, viết luật Cedar duyệt 2 người, chặn rug-pull & SSRF và vận hành HTTP/3 QUIC.",
    ),
    (
        "architecture",
        "concepts/architecture.html",
        "concepts",
        "Three-Plane Architecture",
        "Kiến trúc 3 Mặt phẳng",
        7,
        "Why Execution, Discovery, and Control sit on strictly separated planes backed by lock-free ArcSwap snapshots and AVL OrdMap.",
        "Vì sao Thực thi, Khám phá và Kiểm soát tách thành 3 mặt phẳng độc lập trên nền snapshot ArcSwap và AVL OrdMap.",
    ),
    (
        "progressive-disclosure",
        "concepts/progressive-disclosure.html",
        "concepts",
        "Progressive Disclosure (651 Tokens)",
        "Khám phá Lũy tiến (651 Tokens)",
        6,
        "Walk through L0 __metaGraph, L1 __search, and L2 __type with Cedar visibility bitsets from N=10 to N=1,000 tools.",
        "Theo dõi từng bước L0 __metaGraph, L1 __search và L2 __type kèm visibility bitset ở quy mô 10 đến 1.000 tool.",
    ),
    (
        "gap-ql-and-joins",
        "concepts/gap-ql-and-joins.html",
        "concepts",
        "GAP-QL & Batched Joins",
        "GAP-QL & Gom Lô Join",
        7,
        "Use standard GraphQL queries parsed by apollo-compiler alongside bounded DataLoader batching and honest crossover rules.",
        "Dùng truy vấn GraphQL chuẩn apollo-compiler với cơ chế gom batch DataLoader và bảng ngưỡng hòa vốn minh bạch.",
    ),
    (
        "connectors-overview",
        "connectors/overview.html",
        "connectors",
        "Native Graph Connectors (NGC)",
        "Kết nối Dữ liệu (NGC)",
        7,
        "Connect REST/OpenAPI, SQL, GraphQL, gRPC, sandboxed CLIs, and Code ASTs declaratively with propose-not-activate gates.",
        "Ánh xạ REST/OpenAPI, SQL, GraphQL, gRPC, CLI sandbox và Code AST vào chung hợp đồng NGC kèm duyệt Proposal.",
    ),
    (
        "facades-and-hosts",
        "agents/facades-and-hosts.html",
        "agents",
        "MCP, A2A & AG-UI Facades",
        "Tích hợp MCP, A2A & AG-UI",
        8,
        "Expose one gateway simultaneously over MCP 2026-07-28, A2A v1.0 Agent Cards, and AG-UI v1.0 interactive SSE approvals.",
        "Phục vụ đồng thời giao diện MCP 2026-07-28, tác vụ ngang hàng A2A v1.0 và luồng duyệt AG-UI v1.0 SSE.",
    ),
    (
        "routers-and-http3",
        "agents/routers-and-http3.html",
        "agents",
        "System-1 Routers & HTTP/3 QUIC",
        "Định tuyến System-1 & HTTP/3 QUIC",
        7,
        "Compare measured BASELINE_FALLBACK vs System-1 providers (Laya, Jev, Gemini Decision 1) and dual-stack HTTP/3 QUIC transport.",
        "So sánh thực nghiệm BASELINE_FALLBACK với các adapter System-1 (Laya, Jev, Gemini) và tầng truyền tải HTTP/3 QUIC.",
    ),
    (
        "zero-trust",
        "security/zero-trust.html",
        "security",
        "Zero-Trust Governance & Cedar",
        "Bảo mật Zero-Trust & Cedar",
        8,
        "Configure T0/T1/T2 trust tiers, Cedar two-person mutation rules, DriftMonitor anti-rug-pull freeze, EgressGuard SSRF, and BLAKE3 audits.",
        "Thiết lập phân tầng T0/T1/T2, luật Cedar duyệt 2 người, DriftMonitor chống rug-pull, EgressGuard SSRF và chuỗi BLAKE3.",
    ),
    (
        "playground",
        "lab/playground.html",
        "lab",
        "Interactive Lab & Calculator",
        "Phòng Lab & Bộ tính Token",
        10,
        "Experiment with real GAP-Bench numbers, test live gap serve queries, track your 6-level checklist, and take the 6-question quiz.",
        "Thử nghiệm số liệu thật GAP-Bench, chạy truy vấn gap serve sống, theo dõi checklist 6 cấp và làm bài kiểm tra.",
    ),
    (
        "cli-and-ops",
        "reference/cli-and-ops.html",
        "reference",
        "CLI, Runbooks & 32 Crates",
        "Tra cứu CLI, Runbooks & 32 Crates",
        8,
        "Reference all 15 gap CLI subcommands, 9 Prometheus incident runbooks, 28.86 ms SIGKILL recovery, and the 32-crate DAG map.",
        "Tra cứu trọn bộ 15 lệnh gap CLI, 9 runbook xử lý sự cố Prometheus, phục hồi SIGKILL 28,86 ms và 32 crate Rust.",
    ),
    (
        "invariants-and-glossary",
        "reference/invariants-and-glossary.html",
        "reference",
        "20 Invariants, STE-100 & Paper",
        "20 Bất biến, STE-100 & Bài báo",
        6,
        "Audit INV-01..20 verification status, ASD-STE100 controlled vocabulary, BibTeX citation, and full camera-ready paper PDF/HTML.",
        "Kiểm tra 20 bất biến chuẩn tắc INV-01..20, từ điển kiểm soát ASD-STE100, trích dẫn BibTeX và toàn văn bài báo PDF/HTML.",
    ),
]

PAGES_BY_SLUG = {p[0]: p for p in PAGES}
SECTION_LABELS = {s[0]: {"en": s[1], "vi": s[2]} for s in SECTIONS}

TOP_LINKS = {
    "en": [
        ("start-here", "what-is-gap", "Get Started"),
        ("learn", "learn-graphql-and-gap", "Learn"),
        ("academy", "academy-hub", "Academy"),
        ("concepts", "architecture", "Architecture"),
        ("security", "zero-trust", "Security"),
        ("lab", "playground", "Lab"),
        ("reference", "cli-and-ops", "Reference"),
    ],
    "vi": [
        ("start-here", "what-is-gap", "Bắt đầu"),
        ("learn", "learn-graphql-and-gap", "Kiến thức Nền"),
        ("academy", "academy-hub", "Học viện"),
        ("concepts", "architecture", "Kiến trúc"),
        ("security", "zero-trust", "Bảo mật"),
        ("lab", "playground", "Phòng Lab"),
        ("reference", "cli-and-ops", "Tra cứu"),
    ],
}

CHROME = {
    "en": {
        "site_name": "GAP Docs",
        "version_badge": "v0.1.0-rc.1",
        "skip_text": "Skip to article content",
        "docs_nav_aria": "Documentation sections",
        "top_nav_aria": "Primary sections",
        "lang_nav_aria": "Language",
        "breadcrumb_aria": "Breadcrumbs",
        "breadcrumbs_home": "Docs",
        "drawer_open_aria": "Open documentation menu",
        "drawer_close_aria": "Close documentation menu",
        "drawer_aria": "Documentation navigation drawer",
        "search_label": "Search GAP documentation across all guides",
        "search_placeholder": "Search docs, Academy, INV... [/]",
        "theme_aria": "Toggle Light / Dark theme",
        "theme_dark": "Dark Mode",
        "theme_light": "Light Mode",
        "status_offline": "Static Docs Mode",
        "on_this_page": "On this page",
        "toc_aria": "On this page",
        "copy_md_visible": "Copy as Markdown",
        "read_min_suffix": "min",
        "pager_aria": "Article pagination",
        "prev_label": "Previous",
        "next_label": "Next",
        "footer_note": (
            "Documentation, Learn & Academy grounded 100% in repository source code, specs 00-09, and ADR-021 measurements."
        ),
        "footer_paper": "Paper (HTML)",
        "footer_pdf": "PDF",
        "footer_llms": "llms.txt",
        "footer_other_lang": "Tiếng Việt",
        "other_lang": "vi",
        "other_lang_label": "Tiếng Việt",
        "this_lang_short": "EN",
        "other_lang_short": "VI",
    },
    "vi": {
        "site_name": "Tài liệu GAP",
        "version_badge": "v0.1.0-rc.1",
        "skip_text": "Chuyển tới nội dung bài viết",
        "docs_nav_aria": "Danh mục tài liệu",
        "top_nav_aria": "Nhóm tài liệu chính",
        "lang_nav_aria": "Ngôn ngữ",
        "breadcrumb_aria": "Đường dẫn vị trí",
        "breadcrumbs_home": "Tài liệu",
        "drawer_open_aria": "Mở danh mục tài liệu",
        "drawer_close_aria": "Đóng danh mục tài liệu",
        "drawer_aria": "Ngăn điều hướng tài liệu",
        "search_label": "Tìm kiếm trên toàn bộ tài liệu và Học viện GAP",
        "search_placeholder": "Tìm tài liệu, Học viện, INV... [/]",
        "theme_aria": "Chuyển giao diện Sáng / Tối",
        "theme_dark": "Chế độ Tối",
        "theme_light": "Chế độ Sáng",
        "status_offline": "Chế độ Tĩnh (HTML)",
        "on_this_page": "Trong trang này",
        "toc_aria": "Mục lục bài viết",
        "copy_md_visible": "Sao chép Markdown",
        "read_min_suffix": "phút",
        "pager_aria": "Chuyển bài trước / tiếp",
        "prev_label": "Bài trước",
        "next_label": "Tiếp theo",
        "footer_note": (
            "Tài liệu, Kiến thức Nền & Học viện dựa 100% trên mã nguồn Rust, đặc tả 00-09 và số liệu đo thật ADR-021."
        ),
        "footer_paper": "Bài báo (HTML)",
        "footer_pdf": "PDF",
        "footer_llms": "llms.txt",
        "footer_other_lang": "English",
        "other_lang": "en",
        "other_lang_label": "English",
        "this_lang_short": "VI",
        "other_lang_short": "EN",
    },
}


def page_out_path(slug: str, lang: str) -> str:
    """Return output path relative to `web/` for `slug` and `lang`."""
    rel = PAGES_BY_SLUG[slug][1]
    return rel if lang == "en" else f"vi/{rel}"


def rel_url(from_out_path: str, to_out_path: str) -> str:
    """Compute a clean relative link between two `web/`-relative paths."""
    src_dir = posixpath.dirname(from_out_path) or "."
    return posixpath.relpath(to_out_path, start=src_dir)


def link_to_page(from_slug: str, to_slug: str, lang: str) -> str:
    """Compute relative URL between two pages in the same language."""
    return rel_url(page_out_path(from_slug, lang), page_out_path(to_slug, lang))


def link_to_mirror(slug: str, lang: str) -> str:
    """Compute relative URL to the same slug in the opposite language."""
    other = "vi" if lang == "en" else "en"
    return rel_url(page_out_path(slug, lang), page_out_path(slug, other))


def link_to_asset(from_slug: str, lang: str, asset_rel: str) -> str:
    """Compute relative URL to a shared asset in `web/` (or repo-root `paper/`)."""
    return rel_url(page_out_path(from_slug, lang), asset_rel)


def prev_next(slug: str) -> tuple[str | None, str | None]:
    """Return previous and next slugs in guided learning order."""
    slugs = [p[0] for p in PAGES]
    idx = slugs.index(slug)
    prev_slug = slugs[idx - 1] if idx > 0 else None
    next_slug = slugs[idx + 1] if idx + 1 < len(slugs) else None
    return prev_slug, next_slug
