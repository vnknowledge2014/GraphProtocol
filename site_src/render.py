"""HTML rendering primitives compliant with modern-web-guidance (`copy-button`, `html`, `accessibility`)."""
from __future__ import annotations

import html
import re


def esc(text: str) -> str:
    """Escape HTML special characters safely."""
    return html.escape(text, quote=True)


def inline_code(text: str) -> str:
    """Escape text safely and turn backtick spans into `<code>...</code>`."""
    return re.sub(r"`([^`]+)`", r"<code>\1</code>", esc(text))


def code_block(
    code: str,
    lang_label: str,
    snippet_id: str,
    lang: str = "en",
) -> str:
    """Render an accessible copyable code block with Invoker Command markup."""
    if lang == "vi":
        aria_copy = f"Sao chép đoạn mã {lang_label} ({snippet_id})"
        aria_region = f"Đoạn mã {lang_label}"
    else:
        aria_copy = f"Copy {lang_label} code snippet ({snippet_id})"
        aria_region = f"{lang_label} code block"

    cls = lang_label.lower().replace("/", "").replace(" ", "-")
    return f"""<figure class="code-figure">
  <div class="code-toolbar">
    <span class="code-lang">{esc(lang_label)}</span>
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
    """Render an accessible responsive table with `<th scope="col">` headers."""
    ths = "".join(f'<th scope="col">{esc(h)}</th>' for h in headers)
    trs = []
    for row in rows:
        first = f'<th scope="row">{row[0]}</th>'
        rest = "".join(f"<td>{cell}</td>" for cell in row[1:])
        trs.append(f"<tr>{first}{rest}</tr>")
    tbody = "\n".join(trs)
    cls = f"data-table {extra_cls}".strip()
    return f"""<div tabindex="0" role="region" aria-label="{esc(caption)}" class="table-wrapper">
  <table class="{esc(cls)}">
    <caption>{esc(caption)}</caption>
    <thead><tr>{ths}</tr></thead>
    <tbody>{tbody}</tbody>
  </table>
</div>"""
