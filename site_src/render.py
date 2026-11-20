"""HTML rendering primitives compliant with modern-web-guidance (`copy-button`, `responsive-table`, `html`, `accessibility`)."""
from __future__ import annotations

import hashlib
import html
import re


def esc(text: str) -> str:
    """Escape HTML special characters safely."""
    return html.escape(text, quote=True)


def inline_code(text: str) -> str:
    """Escape text safely and turn backtick spans into `<code>...</code>`."""
    return re.sub(r"`([^`]+)`", r"<code>\1</code>", esc(text))


def _css_str(text: str) -> str:
    """Escape a string literal safely for inline CSS custom properties."""
    return text.replace("\\", "\\\\").replace('"', '\\"').replace("\n", " ")


def code_block(
    code: str,
    lang_label: str,
    snippet_id: str,
    lang: str = "en",
    title: str = "",
) -> str:
    """Render an accessible copyable code block with Invoker Command markup."""
    if lang == "vi":
        aria_copy = f"Sao chép đoạn mã {lang_label} ({title or snippet_id})"
        aria_region = title or f"Đoạn mã {lang_label}"
    else:
        aria_copy = f"Copy {lang_label} code snippet ({title or snippet_id})"
        aria_region = title or f"{lang_label} code block"

    cls = lang_label.lower().replace("/", "").replace(" ", "-")
    title_html = f'<span class="code-title">{esc(title)}</span>' if title else ""
    return f"""<figure class="code-figure">
  <div class="code-toolbar">
    <div class="code-meta">
      <span class="code-lang">{esc(lang_label)}</span>
      {title_html}
    </div>
    <span class="copy">
      <button type="button" class="copy-button" command="--copy" commandfor="{esc(snippet_id)}" aria-label="{esc(aria_copy)}">
        <span class="icon" aria-hidden="true"></span>
      </button>
      <span class="tip" role="status"></span>
    </span>
  </div>
  <pre role="region" aria-label="{esc(aria_region)}" tabindex="0"><code id="{esc(snippet_id)}" class="language-{esc(cls)}">{esc(code.strip())}</code></pre>
</figure>"""


def callout(kind: str, title: str, body_html: str) -> str:
    """Render a semantic callout box (`note`, `tip`, `important`, `tradeoff`)."""
    return f"""<aside class="callout callout-{esc(kind)}">
  <p class="callout-title">{esc(title)}</p>
  <div class="callout-body">{body_html}</div>
</aside>"""


def step_item(num: int, title: str, body_html: str) -> str:
    """Render one numbered tutorial step."""
    return f"""<li class="step-item">
  <div class="step-badge" aria-hidden="true">{num}</div>
  <div class="step-content">
    <h3>{inline_code(title)}</h3>
    {body_html}
  </div>
</li>"""


def steps_list(items: list[tuple[str, str]]) -> str:
    """Render an Apollo-style numbered step-by-step guide list."""
    rendered = "\n".join(step_item(i + 1, t, b) for i, (t, b) in enumerate(items))
    return f'<ol class="steps-list">\n{rendered}\n</ol>'


def data_table(caption: str, headers: list[str], rows: list[list[str]], extra_cls: str = "") -> str:
    """Render an accessible responsive table with `<th scope="col">` headers and mobile CSS vars."""
    cap_id = "cap-" + hashlib.sha256(caption.encode("utf-8")).hexdigest()[:8]
    ths = "".join(f'<th scope="col">{esc(h)}</th>' for h in headers)
    trs = []
    for row in rows:
        first = f'<th scope="row">{row[0]}</th>'
        rest = "".join(f"<td>{cell}</td>" for cell in row[1:])
        trs.append(f"<tr>{first}{rest}</tr>")
    tbody = "\n".join(trs)
    col_vars = " ".join(
        f'--col-{idx + 2}: "{_css_str(h)}";' for idx, h in enumerate(headers[1:])
    )
    style_attr = f' style="{esc(col_vars)}"' if col_vars else ""
    cls = f"data-table {extra_cls}".strip()
    return f"""<div tabindex="0" role="region" aria-labelledby="{esc(cap_id)}" class="table-wrapper">
  <table class="{esc(cls)}"{style_attr}>
    <caption id="{esc(cap_id)}">{esc(caption)}</caption>
    <thead><tr>{ths}</tr></thead>
    <tbody>{tbody}</tbody>
  </table>
</div>"""
