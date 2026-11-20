#!/usr/bin/env python3
"""Static site generator and validator for the GraphProtocol bilingual (EN/VI) portal.

Usage:
  python3 web/build_site.py          # Render HTML pages, search indices, vi.html, and llms.txt
  python3 web/build_site.py --check  # Validate rendered site (headings, links, fragments, parity)
"""
from __future__ import annotations

import argparse
from html.parser import HTMLParser
import json
from pathlib import Path
import posixpath
import re
import sys
import xml.etree.ElementTree as ET

WEB_DIR = Path(__file__).resolve().parent
REPO_ROOT = WEB_DIR.parent

if str(WEB_DIR) not in sys.path:
    sys.path.insert(0, str(WEB_DIR))

from site_src.nav import (  # noqa: E402
    CHROME,
    PAGES,
    PAGES_BY_SLUG,
    SECTION_LABELS,
    SECTIONS,
    TOP_LINKS,
    link_to_asset,
    link_to_mirror,
    link_to_page,
    page_out_path,
    prev_next,
)
from site_src.pages_academy_en import build_academy_en  # noqa: E402
from site_src.pages_academy_vi import build_academy_vi  # noqa: E402
from site_src.pages_en import build_en_part1  # noqa: E402
from site_src.pages_en_part2 import build_en_part2  # noqa: E402
from site_src.pages_vi import build_vi_part1  # noqa: E402
from site_src.pages_vi_part2 import build_vi_part2  # noqa: E402
from site_src.render import esc, inline_code  # noqa: E402

INTERACTIVE_SLUGS = {
    "architecture",
    "playground",
    "cli-and-ops",
    "invariants-and-glossary",
}

EXPECTED_PAGE_IDS = {
    "learn-graphql-and-gap": {"query-builder-widget", "qb-graphql-code", "qb-json-code"},
    "academy-hub": {"academy-hub-progress", "academy-total-count", "academy-total-fill"},
    "architecture": {"planes-grid", "arch-inspector"},
    "playground": {"calc-suite", "levels-tablist", "quiz-container"},
    "cli-and-ops": {"cli-list", "runbooks-accordion", "layer-chips", "crates-grid"},
    "invariants-and-glossary": {"inv-tbody", "dict-tbody"},
}


def strip_html_text(raw_html: str) -> str:
    """Collapse HTML markup into clean plain text for client-side search snippets."""
    no_code = re.sub(r"<(pre|script|style)\b[^>]*>.*?</\1>", " ", raw_html, flags=re.S)
    text = re.sub(r"<[^>]+>", " ", no_code)
    text = re.sub(r"\s+", " ", text).strip()
    return text


def first_slug_in_section(section_id: str) -> str:
    """Return the landing page slug for a navigation section."""
    for p_slug, _rel, sec_id, *_rest in PAGES:
        if sec_id == section_id:
            return p_slug
    return "home"


def root_prefix_for(out_path: str) -> str:
    """Return trailing-slash prefix from an output file back to `web/` root."""
    depth = out_path.count("/")
    return "../" * depth


def render_nav_tree(current_slug: str, lang: str, id_prefix: str = "side") -> str:
    """Render the 7-section sidebar navigation list cleanly for desktop or mobile drawer."""
    title_idx = 3 if lang == "en" else 4
    groups = []
    for sec_id, label_en, label_vi in SECTIONS:
        sec_label = label_en if lang == "en" else label_vi
        items_html = []
        for p in PAGES:
            p_slug, _rel, p_sec_id = p[0], p[1], p[2]
            if p_sec_id != sec_id:
                continue
            p_title = p[title_idx]
            href = link_to_page(current_slug, p_slug, lang)
            current_attr = ' aria-current="page"' if p_slug == current_slug else ""
            cls = "nav-link is-active" if p_slug == current_slug else "nav-link"
            items_html.append(
                f'<li><a class="{cls}" href="{esc(href)}"{current_attr}>{esc(p_title)}</a></li>'
            )
        groups.append(
            f"""<div class="nav-group" data-section-id="{esc(sec_id)}">
  <p class="nav-group-title" id="{esc(id_prefix)}-{esc(sec_id)}">{esc(sec_label)}</p>
  <ul aria-labelledby="{esc(id_prefix)}-{esc(sec_id)}">
    {"".join(items_html)}
  </ul>
</div>"""
        )
    return "\n".join(groups)


def render_page(slug: str, lang: str, page_data: dict) -> str:
    """Render one complete accessible HTML page for `(slug, lang)`."""
    _s, _rel, section_id, title_en, title_vi, read_min, sum_en, sum_vi = PAGES_BY_SLUG[slug]
    c = CHROME[lang]
    title = title_en if lang == "en" else title_vi
    summary = sum_en if lang == "en" else sum_vi
    section_label = SECTION_LABELS[section_id][lang]
    out_path = page_out_path(slug, lang)
    root_prefix = root_prefix_for(out_path)

    u = lambda target: link_to_page(slug, target, lang)
    a = lambda asset: link_to_asset(slug, lang, asset)
    mirror_href = link_to_mirror(slug, lang)
    en_alt = link_to_asset(slug, lang, page_out_path(slug, "en"))
    vi_alt = link_to_asset(slug, lang, page_out_path(slug, "vi"))

    # Inline data-URI favicon (zero external network fetches, passes offline --check)
    favicon_uri = (
        "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E"
        "%3Crect width='32' height='32' rx='8' fill='%230f4c81'/%3E"
        "%3Ctext x='16' y='22' text-anchor='middle' font-family='system-ui,sans-serif' "
        "font-weight='800' font-size='18' fill='%23ffffff'%3EG%3C/text%3E%3C/svg%3E"
    )

    # Top-bar links (single compact horizontal row)
    top_nav_items = []
    for sec_id, target_slug, label in TOP_LINKS[lang]:
        active = ' aria-current="true" class="top-link is-active"' if sec_id == section_id else ' class="top-link"'
        top_nav_items.append(f'<a href="{esc(u(target_slug))}"{active}>{esc(label)}</a>')

    # Breadcrumbs: each <li> holds an <a> per modern-web-guidance `breadcrumbs`
    section_home = first_slug_in_section(section_id)
    breadcrumbs_html = f"""<nav class="breadcrumb" aria-label="{esc(c['breadcrumb_aria'])}">
  <ol>
    <li><a href="{esc(u('home'))}">{esc(c['breadcrumbs_home'])}</a></li>
    <li><a href="{esc(u(section_home))}">{esc(section_label)}</a></li>
    <li><a href="{esc(u(slug))}" aria-current="page">{esc(title)}</a></li>
  </ol>
</nav>"""

    # "What you'll learn" box (using <h2> so headings go h1 -> h2 -> h3 cleanly)
    learn_heading = "What you'll learn" if lang == "en" else "Bạn sẽ nắm được gì"
    bullets_html = "".join(f"<li>{inline_code(b)}</li>" for b in page_data["learn_bullets"])
    learn_card = f"""<section class="learn-card" aria-labelledby="learn-heading">
  <h2 id="learn-heading">{esc(learn_heading)}</h2>
  <ul>{bullets_html}</ul>
</section>"""

    # Main content sections + Right TOC entries
    section_blocks = []
    toc_links = []
    toc_inline_links = []
    for sec in page_data["sections"]:
        sid = sec["id"]
        h2_raw = sec["h2"]
        h2_html = inline_code(h2_raw)
        section_blocks.append(
            f'<section id="{esc(sid)}" tabindex="-1" class="doc-section">\n'
            f"  <h2>{h2_html}</h2>\n"
            f'  {sec["html"]}\n'
            f"</section>"
        )
        toc_links.append(f'<a class="toc-link" href="#{esc(sid)}">{h2_html}</a>')
        toc_inline_links.append(f'<a class="toc-inline-link" href="#{esc(sid)}">{h2_html}</a>')

    toc_inline_html = (
        f'<details class="toc-inline"><summary>{esc(c["on_this_page"])}</summary>'
        f'<nav class="toc-inline-links" aria-label="{esc(c["toc_aria"])}">{"".join(toc_inline_links)}</nav>'
        f"</details>"
    )

    # Prev / Next pagination cards
    prev_slug, next_slug = prev_next(slug)
    pager_cards = []
    if prev_slug:
        pt = PAGES_BY_SLUG[prev_slug][3 if lang == "en" else 4]
        pager_cards.append(
            f'<a class="pager-card pager-prev" href="{esc(u(prev_slug))}">'
            f'<span class="pager-dir">&larr; {esc(c["prev_label"])}</span>'
            f'<span class="pager-title">{esc(pt)}</span></a>'
        )
    else:
        pager_cards.append('<span class="pager-spacer" aria-hidden="true"></span>')

    if next_slug:
        nt = PAGES_BY_SLUG[next_slug][3 if lang == "en" else 4]
        pager_cards.append(
            f'<a class="pager-card pager-next" href="{esc(u(next_slug))}">'
            f'<span class="pager-dir">{esc(c["next_label"])} &rarr;</span>'
            f'<span class="pager-title">{esc(nt)}</span></a>'
        )
    else:
        pager_cards.append('<span class="pager-spacer" aria-hidden="true"></span>')

    pager_html = f'<nav class="doc-pager" aria-label="{esc(c["pager_aria"])}">{"".join(pager_cards)}</nav>'

    # Scripts: always search index + app.js; load full dataset only on 4 interactive pages
    extra_data_script = (
        f'  <script src="{esc(a(f"data.{lang}.js"))}"></script>\n'
        if slug in INTERACTIVE_SLUGS
        else ""
    )

    desktop_nav_html = render_nav_tree(slug, lang, "side")
    drawer_nav_html = render_nav_tree(slug, lang, "drawer")

    return f"""<!DOCTYPE html>
<html lang="{esc(lang)}">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <meta name="color-scheme" content="light dark" />
  <script>
    try {{
      var s = localStorage.getItem("gap-color-scheme");
      if (s === "light" || s === "dark") {{
        document.querySelector('meta[name="color-scheme"]').content = s;
      }}
    }} catch (_e) {{}}
  </script>
  <title>{esc(title)} — {esc(c['site_name'])}</title>
  <meta name="description" content="{esc(summary)}" />
  <link rel="canonical" href="https://vnknowledge2014.github.io/GraphProtocol/{esc(out_path)}" />
  <link rel="alternate" hreflang="en" href="{esc(en_alt)}" />
  <link rel="alternate" hreflang="vi" href="{esc(vi_alt)}" />
  <link rel="icon" type="image/svg+xml" href="{esc(favicon_uri)}" />
  <link rel="stylesheet" href="{esc(a('styles.css'))}" />
</head>
<body data-slug="{esc(slug)}" data-lang="{esc(lang)}" data-section="{esc(section_id)}" data-web-root="{esc(root_prefix)}">
  <a class="skip-link" href="#article-title">{esc(c['skip_text'])}</a>

  <header class="topbar">
    <div class="topbar-inner">
      <div class="topbar-left">
        <button type="button" id="drawer-open" class="drawer-open-btn" aria-label="{esc(c['drawer_open_aria'])}" aria-controls="docs-drawer">
          <svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true">
            <path d="M4 6h16M4 12h16M4 18h16" />
          </svg>
        </button>
        <a class="brand-link" href="{esc(u('home'))}">
          <span class="brand-mark" aria-hidden="true">G</span>
          <span class="brand-text"><strong>GAP</strong> <span class="brand-sub">{esc(c['site_name'])}</span></span>
          <span class="badge-ver">{esc(c['version_badge'])}</span>
        </a>
      </div>

      <nav class="top-nav" aria-label="{esc(c['top_nav_aria'])}">
        {"".join(top_nav_items)}
      </nav>

      <div class="topbar-controls">
        <search class="search-wrap">
          <label class="visually-hidden" for="global-search">{esc(c['search_label'])}</label>
          <input id="global-search" type="search" placeholder="{esc(c['search_placeholder'])}" autocomplete="off" />
          <div id="search-results" class="search-results scroller" role="region" aria-live="polite" hidden></div>
        </search>
        <nav class="lang-switch" aria-label="{esc(c['lang_nav_aria'])}">
          <span class="lang-current" aria-current="true">{esc(c['this_lang_short'])}</span>
          <a class="lang-alt" href="{esc(mirror_href)}" hreflang="{esc(c['other_lang'])}" lang="{esc(c['other_lang'])}">{esc(c['other_lang_short'])}</a>
        </nav>
        <button type="button" id="theme-toggle-btn" class="theme-toggle" aria-label="{esc(c['theme_aria'])}">
          <span class="icon-to-dark" aria-hidden="true">☽</span>
          <span class="icon-to-light" aria-hidden="true">☀</span>
        </button>
      </div>
    </div>
  </header>

  <dialog class="Drawer scroller" id="docs-drawer" aria-label="{esc(c['drawer_aria'])}">
    <nav class="Drawer-nav scroller" aria-label="{esc(c['docs_nav_aria'])}">
      <div class="Drawer-head">
        <span class="Drawer-brand"><strong>GAP</strong> {esc(c['site_name'])}</span>
        <button type="button" id="drawer-close" class="drawer-close-btn" aria-label="{esc(c['drawer_close_aria'])}">&times;</button>
      </div>
      {drawer_nav_html}
    </nav>
    <button type="button" id="drawer-backdrop-close" class="Drawer-backdrop" aria-label="{esc(c['drawer_close_aria'])}"></button>
  </dialog>

  <div class="docs-shell">
    <aside class="sidebar scroller" aria-label="{esc(c['docs_nav_aria'])}">
      <nav aria-label="{esc(c['docs_nav_aria'])}">
        {desktop_nav_html}
      </nav>
      <div class="sidebar-status" role="status" aria-live="polite">
        <span id="server-status-dot" class="status-dot" aria-hidden="true"></span>
        <span id="server-status-text">{esc(c['status_offline'])}</span>
      </div>
    </aside>

    <main id="main-content" class="doc-main">
      <article class="doc-article">
        {breadcrumbs_html}
        <div class="article-meta-bar">
          <span class="read-time">{read_min} {esc(c['read_min_suffix'])}</span>
          <span class="copy copy-md-wrap">
            <button type="button" class="copy-button copy-md-btn" command="--copy" commandfor="page-markdown-src">
              <span class="icon" aria-hidden="true"></span>
              <span class="btn-label">{esc(c['copy_md_visible'])}</span>
            </button>
            <span class="tip" role="status"></span>
          </span>
        </div>

        <textarea id="page-markdown-src" class="visually-hidden" readonly tabindex="-1" aria-hidden="true">{esc(page_data['markdown'].strip())}</textarea>

        <h1 id="article-title" tabindex="-1">{esc(title)}</h1>
        <p class="article-lead">{inline_code(page_data['lead'])}</p>

        {learn_card}
        {toc_inline_html}

        {"".join(section_blocks)}

        {pager_html}
      </article>

      <footer class="doc-footer">
        <p class="footer-note">{esc(c['footer_note'])}</p>
        <div class="footer-links">
          <a href="{esc(a('paper/gap-paper.html'))}">{esc(c['footer_paper'])}</a>
          <a href="{esc(a('paper/gap-paper.pdf'))}">{esc(c['footer_pdf'])}</a>
          <a href="{esc(a('llms.txt'))}">{esc(c['footer_llms'])}</a>
          <a href="{esc(mirror_href)}" hreflang="{esc(c['other_lang'])}" lang="{esc(c['other_lang'])}">{esc(c['footer_other_lang'])}</a>
        </div>
      </footer>
    </main>

    <aside class="toc-right" aria-label="{esc(c['toc_aria'])}">
      <div class="toc-sticky scroller">
        <p class="toc-title">{esc(c['on_this_page'])}</p>
        <nav id="on-this-page-nav" class="toc-links" aria-label="{esc(c['toc_aria'])}">
          {"".join(toc_links)}
        </nav>
      </div>
    </aside>
  </div>

  <script src="{esc(a(f'search-index.{lang}.js'))}"></script>
{extra_data_script}  <script src="{esc(a('app.js'))}"></script>
</body>
</html>
"""


def render_vi_redirect() -> str:
    """Render `web/vi.html` preserving direct `/vi.html` links with clean redirect to `vi/index.html`."""
    return """<!DOCTYPE html>
<html lang="vi">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <meta name="color-scheme" content="light dark" />
  <meta http-equiv="refresh" content="0; url=./vi/index.html" />
  <title>Chuyển tới Tài liệu GAP Tiếng Việt — GAP Docs</title>
  <link rel="canonical" href="./vi/index.html" />
  <link rel="alternate" hreflang="en" href="./index.html" />
  <link rel="alternate" hreflang="vi" href="./vi/index.html" />
  <link rel="stylesheet" href="./styles.css" />
</head>
<body class="redirect-body">
  <main class="redirect-main">
    <h1>Tài liệu GAP Tiếng Việt</h1>
    <p>Cổng tài liệu tiếng Việt đã được chuyển sang <a href="./vi/index.html">vi/index.html</a> với đầy đủ 20 trang hướng dẫn, Kiến thức Nền &amp; Học viện GAP.</p>
    <p><a class="btn btn-primary" href="./vi/index.html">Mở Tài liệu GAP Tiếng Việt</a> <a class="btn" href="./index.html" hreflang="en" lang="en">English Docs</a></p>
  </main>
  <script>location.replace("./vi/index.html");</script>
</body>
</html>
"""


def build_search_index(lang: str, pages_dict: dict[str, dict]) -> list[dict]:
    """Build compact full-site search index array for one language."""
    title_idx = 3 if lang == "en" else 4
    sum_idx = 6 if lang == "en" else 7
    entries = []
    for p in PAGES:
        slug, _rel, sec_id = p[0], p[1], p[2]
        out_path = page_out_path(slug, lang)
        sec_label = SECTION_LABELS[sec_id][lang]
        page_title = p[title_idx]
        summary = p[sum_idx]
        pdata = pages_dict[slug]
        entries.append(
            {
                "title": page_title,
                "section": sec_label,
                "href": out_path,
                "snippet": summary,
                "keywords": " ".join([page_title, summary, pdata["lead"]]),
            }
        )
        for sec in pdata["sections"]:
            plain = strip_html_text(sec["html"])
            h2_clean = re.sub(r"`([^`]+)`", r"\1", sec["h2"])
            entries.append(
                {
                    "title": f"{h2_clean} — {page_title}",
                    "section": sec_label,
                    "href": f'{out_path}#{sec["id"]}',
                    "snippet": plain[:190],
                    "keywords": f"{h2_clean} {page_title} {plain[:450]}",
                }
            )
    return entries


def render_llms_txt() -> str:
    """Render Apollo-style `/llms.txt` directory covering both English and Vietnamese docs."""
    base = "https://vnknowledge2014.github.io/GraphProtocol"
    lines = [
        "# Graph Agent Protocol (GAP) Documentation",
        "",
        "> Typed GraphQL-profile gateway, progressive discovery, batched federation, and zero-trust governance runtime in Rust (32 crates).",
        "",
        "## English Documentation",
        "",
    ]
    for p in PAGES:
        slug, rel, _sec, title_en, _tvi, _min, sum_en, _svi = p
        lines.append(f"- [{title_en}]({base}/{rel}): {sum_en}")
    lines.extend(["", "## Tài liệu Tiếng Việt (Vietnamese)", ""])
    for p in PAGES:
        slug, rel, _sec, _ten, title_vi, _min, _sen, sum_vi = p
        lines.append(f"- [{title_vi}]({base}/vi/{rel}): {sum_vi}")
    lines.extend(
        [
            "",
            "## Academic Paper & Citation",
            "",
            f"- [GAP Academic Paper (HTML)]({base}/paper/gap-paper.html)",
            f"- [GAP Academic Paper (PDF)]({base}/paper/gap-paper.pdf)",
            f"- [CITATION.cff]({base}/CITATION.cff)",
            "",
        ]
    )
    return "\n".join(lines)


class HtmlAuditParser(HTMLParser):
    """Strict HTML auditor checking heading outline, lang, alternates, IDs, and links."""

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.html_lang: str | None = None
        self.has_title = False
        self.alternates: dict[str, str] = {}
        self.headings: list[int] = []
        self.ids: set[str] = set()
        self.links: list[tuple[str, str]] = []  # (tag, target)
        self.in_raw_tag = 0

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        amap = {k: (v or "") for k, v in attrs}
        if "id" in amap and amap["id"]:
            self.ids.add(amap["id"])
        if tag == "html":
            self.html_lang = amap.get("lang")
        elif tag == "title":
            self.has_title = True
        elif tag == "link":
            rel = amap.get("rel", "").split()
            href = amap.get("href", "")
            if "alternate" in rel and amap.get("hreflang"):
                self.alternates[amap["hreflang"]] = href
            if href and "canonical" not in rel:
                self.links.append(("link", href))
        elif tag in ("a",):
            href = amap.get("href", "")
            if href:
                self.links.append(("a", href))
        elif tag in ("script", "img"):
            src = amap.get("src", "")
            if src:
                self.links.append((tag, src))
        elif tag in ("h1", "h2", "h3", "h4", "h5", "h6"):
            self.headings.append(int(tag[1]))
        if tag in ("pre", "code", "textarea", "script", "style"):
            self.in_raw_tag += 1

    def handle_endtag(self, tag: str) -> None:
        if tag in ("pre", "code", "textarea", "script", "style") and self.in_raw_tag > 0:
            self.in_raw_tag -= 1


def write_site() -> int:
    """Generate all static HTML pages, search indices, redirect, and `llms.txt`."""
    data_en = build_en_part1() | build_academy_en() | build_en_part2()
    data_vi = build_vi_part1() | build_academy_vi() | build_vi_part2()

    expected_slugs = [p[0] for p in PAGES]
    assert set(data_en.keys()) == set(expected_slugs), f"EN page keys mismatch: {set(data_en.keys()) ^ set(expected_slugs)}"
    assert set(data_vi.keys()) == set(expected_slugs), f"VI page keys mismatch: {set(data_vi.keys()) ^ set(expected_slugs)}"

    written = 0
    for lang, pdata in (("en", data_en), ("vi", data_vi)):
        for slug in expected_slugs:
            out_file = WEB_DIR / page_out_path(slug, lang)
            out_file.parent.mkdir(parents=True, exist_ok=True)
            out_file.write_text(render_page(slug, lang, pdata[slug]), encoding="utf-8")
            written += 1

        idx = build_search_index(lang, pdata)
        idx_file = WEB_DIR / f"search-index.{lang}.js"
        idx_file.write_text(
            "// Auto-generated by web/build_site.py\n"
            f"window.GAP_SEARCH_INDEX = {json.dumps(idx, ensure_ascii=False, indent=2)};\n",
            encoding="utf-8",
        )
        written += 1

    (WEB_DIR / "vi.html").write_text(render_vi_redirect(), encoding="utf-8")
    (WEB_DIR / "llms.txt").write_text(render_llms_txt(), encoding="utf-8")
    written += 2
    return written


def audit_svg_geometry(html_text: str, rel_disp: str) -> list[str]:
    """Verify inline SVG diagram `<text>` bounding boxes never overflow their `viewBox` or container `<rect>`."""
    issues: list[str] = []
    for raw_svg in re.findall(r"<svg\b[^>]*>.*?</svg>", html_text, flags=re.S):
        try:
            root = ET.fromstring(raw_svg)
        except ET.ParseError as exc:
            issues.append(f"{rel_disp}: invalid SVG markup ({exc})")
            continue
        vb = root.attrib.get("viewBox", "")
        parts = vb.split()
        if len(parts) != 4:
            continue
        vb_x, _vb_y, vb_w, _vb_h = [float(p) for p in parts]
        if vb_w <= 48:
            continue

        rects: list[tuple[float, float, float, float]] = []
        for el in root.iter():
            if el.tag.endswith("rect") and all(k in el.attrib for k in ("x", "y", "width", "height")):
                rects.append(
                    (
                        float(el.attrib["x"]),
                        float(el.attrib["y"]),
                        float(el.attrib["width"]),
                        float(el.attrib["height"]),
                    )
                )

        for el in root.iter():
            if not el.tag.endswith("text"):
                continue
            txt = "".join(el.itertext()).strip()
            if not txt:
                continue
            tx = float(el.attrib.get("x", 0))
            ty = float(el.attrib.get("y", 0))
            fs = float(el.attrib.get("font-size", 12))
            anchor = el.attrib.get("text-anchor", "start")
            est_w = len(txt) * fs * 0.58
            if anchor == "middle":
                x_left, x_right = tx - est_w / 2.0, tx + est_w / 2.0
            elif anchor == "end":
                x_left, x_right = tx - est_w, tx
            else:
                x_left, x_right = tx, tx + est_w

            if x_left < vb_x or x_right > vb_x + vb_w:
                issues.append(
                    f"{rel_disp}: SVG text {txt!r} ([{x_left:.1f}, {x_right:.1f}]) exceeds viewBox [0, {vb_w:.0f}]"
                )
                continue

            enclosing = [
                (rx, ry, rw, rh)
                for (rx, ry, rw, rh) in rects
                if rx <= tx <= rx + rw and ry <= ty <= ry + rh
            ]
            if enclosing:
                rx, _ry, rw, _rh = min(enclosing, key=lambda r: r[2] * r[3])
                if x_left < rx + 2.0 or x_right > rx + rw - 2.0:
                    issues.append(
                        f"{rel_disp}: SVG text {txt!r} ([{x_left:.1f}, {x_right:.1f}]) overflows rect [{rx:.1f}, {rx + rw:.1f}]"
                    )
    return issues


def validate_site() -> list[str]:
    """Run strict offline checks on every generated HTML page and shared asset link."""
    errors: list[str] = []
    files_to_check: list[tuple[Path, str, str | None]] = []

    for p in PAGES:
        slug = p[0]
        files_to_check.append((WEB_DIR / page_out_path(slug, "en"), "en", slug))
        files_to_check.append((WEB_DIR / page_out_path(slug, "vi"), "vi", slug))
    files_to_check.append((WEB_DIR / "vi.html", "vi", None))

    parsed_cache: dict[Path, HtmlAuditParser] = {}
    for fpath, expected_lang, slug in files_to_check:
        if not fpath.is_file():
            errors.append(f"Missing output file: {fpath.relative_to(REPO_ROOT)}")
            continue
        html_text = fpath.read_text(encoding="utf-8")
        parser = HtmlAuditParser()
        parser.feed(html_text)
        parsed_cache[fpath] = parser

        rel_disp = str(fpath.relative_to(WEB_DIR))
        if parser.html_lang != expected_lang:
            errors.append(f"{rel_disp}: expected lang={expected_lang!r}, got {parser.html_lang!r}")
        if not parser.has_title:
            errors.append(f"{rel_disp}: missing <title>")
        if "en" not in parser.alternates or "vi" not in parser.alternates:
            errors.append(f"{rel_disp}: missing hreflang alternates (en/vi)")

        h1_count = sum(1 for h in parser.headings if h == 1)
        if h1_count != 1:
            errors.append(f"{rel_disp}: expected 1 <h1>, found {h1_count}")
        prev = 0
        for lvl in parser.headings:
            if prev and lvl > prev + 1:
                errors.append(f"{rel_disp}: skipped heading level h{prev} -> h{lvl}")
            prev = lvl

        if slug and slug in EXPECTED_PAGE_IDS:
            missing_ids = EXPECTED_PAGE_IDS[slug] - parser.ids
            if missing_ids:
                errors.append(f"{rel_disp}: missing interactive hook IDs {sorted(missing_ids)}")

        # Check stray backticks outside code/pre/textarea/script/style
        stripped_text = re.sub(
            r"<(pre|code|textarea|script|style)\b[^>]*>.*?</\1>",
            "",
            html_text,
            flags=re.S,
        )
        if "`" in stripped_text:
            errors.append(f"{rel_disp}: stray backtick character found outside <code>/<pre>")

        errors.extend(audit_svg_geometry(html_text, rel_disp))

    # Second pass: resolve every relative href/src and optional #fragment
    for fpath, _lang, _slug in files_to_check:
        if fpath not in parsed_cache:
            continue
        rel_disp = str(fpath.relative_to(WEB_DIR))
        cur_web_dir = str(fpath.parent.relative_to(WEB_DIR))
        cur_web_dir = "" if cur_web_dir == "." else cur_web_dir

        for tag, raw_target in parsed_cache[fpath].links:
            if raw_target.startswith(("http://", "https://", "data:", "mailto:", "javascript:")):
                continue
            path_part, _, frag = raw_target.partition("#")
            if not path_part:
                target_file = fpath
            else:
                norm = posixpath.normpath(
                    posixpath.join(cur_web_dir, path_part) if cur_web_dir else path_part
                )
                if norm.startswith(".."):
                    errors.append(f"{rel_disp}: escaping relative {tag} target {raw_target!r}")
                    continue
                target_file = WEB_DIR / norm
                if not target_file.exists():
                    # Allow paper/, media/, CITATION.cff hosted at site root on gh-pages and serve.py
                    repo_fallback = REPO_ROOT / norm
                    if repo_fallback.exists():
                        if repo_fallback.is_dir():
                            # e.g. paper/ maps to paper/gap-paper.html in serve.py & gh-pages redirect/index
                            continue
                        continue
                    errors.append(f"{rel_disp}: broken {tag} target {raw_target!r} -> {norm}")
                    continue
            if frag and target_file.suffix == ".html":
                if target_file not in parsed_cache:
                    p2 = HtmlAuditParser()
                    p2.feed(target_file.read_text(encoding="utf-8"))
                    parsed_cache[target_file] = p2
                if frag not in parsed_cache[target_file].ids:
                    errors.append(
                        f"{rel_disp}: missing anchor #{frag} in {target_file.relative_to(WEB_DIR)}"
                    )

    for aux in ("search-index.en.js", "search-index.vi.js", "llms.txt"):
        if not (WEB_DIR / aux).is_file():
            errors.append(f"Missing generated asset: web/{aux}")

    # Verify exported Markdown blocks do not contain broken relative *.md links (docs pages use *.html)
    data_en = build_en_part1() | build_academy_en() | build_en_part2()
    data_vi = build_vi_part1() | build_academy_vi() | build_vi_part2()
    for lcode, dset in (("en", data_en), ("vi", data_vi)):
        for slug, pdata in dset.items():
            md = pdata.get("markdown", "")
            for m in re.finditer(r"\]\((?!https?://|docs/|spec/|examples/)([^)]+\.md)\)", md):
                errors.append(f"{lcode}/{slug} markdown has non-HTML relative link {m.group(1)!r}")

    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description="Build and validate the GraphProtocol bilingual web docs.")
    parser.add_argument("--check", action="store_true", help="Validate generated site without rewriting.")
    args = parser.parse_args()

    if not args.check:
        n = write_site()
        print(f"Generated {n} files under {WEB_DIR}")

    errs = validate_site()
    if errs:
        print(f"Validation FAILED ({len(errs)} issues):", file=sys.stderr)
        for e in errs:
            print(f"  - {e}", file=sys.stderr)
        return 1
    print(f"Validation PASSED: {len(PAGES) * 2} bilingual pages + vi.html + search indices + llms.txt + SVG geometry verified (0 errors).")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
