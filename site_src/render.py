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


def going_deeper(title: str, body_html: str) -> str:
    """Render an expandable 'Going deeper' disclosure (`graphql.com/learn` pattern)."""
    return f"""<details class="going-deeper">
  <summary>{inline_code(title)}</summary>
  <div class="details-body">{body_html}</div>
</details>"""


def metric_bar_chart(
    chart_id: str,
    title: str,
    subtitle: str,
    rows: list[tuple[str, float, str, str]],
    max_val: float,
    lang: str = "en",
) -> str:
    """Render a responsive HTML/CSS horizontal bar comparison chart paired with metric labels."""
    denom = max(float(max_val), 1.0)
    note = (
        "Visual bar comparison (min-width clamped for clarity on log-scale ratios)."
        if lang == "en"
        else "Biểu đồ thanh trực quan (giữ tối thiểu 6px để nhận diện tỷ lệ chênh lệch lớn)."
    )
    items = []
    for label_html, num_val, val_text, variant in rows:
        pct = 0.0 if num_val <= 0 else max(2.5, min(100.0, (float(num_val) * 100.0) / denom))
        vcls = "is-warn" if variant == "warn" else ("is-ok" if variant == "ok" else "is-accent")
        items.append(
            f"""<div class="metric-row">
    <span class="metric-label">{label_html}</span>
    <div class="metric-track" role="img" aria-label="{esc(val_text)}">
      <div class="metric-bar-fill {vcls}" style="inline-size:{pct:.1f}%"></div>
    </div>
    <strong class="metric-val">{esc(val_text)}</strong>
  </div>"""
        )
    return f"""<figure class="metric-chart" id="chart-{esc(chart_id)}" aria-labelledby="{esc(chart_id)}-cap">
  <figcaption id="{esc(chart_id)}-cap" class="metric-chart-head">
    <strong>{inline_code(title)}</strong>
    <span>{inline_code(subtitle)}</span>
  </figcaption>
  <div class="metric-rows">
    {"".join(items)}
  </div>
  <p class="metric-footnote">{esc(note)}</p>
</figure>"""


def code_challenge(
    cid: str,
    title: str,
    prompt_html: str,
    code_template: str,
    expected_answers: list[str],
    hints: list[str],
    explain_ok: str,
    lang: str = "en",
) -> str:
    """Render an interactive Odyssey-style Fill-in-the-Blank challenge."""
    badge = "Code Challenge" if lang == "en" else "Thử thách Điền mã"
    lbl_text = "Type the missing token/field:" if lang == "en" else "Nhập từ khóa / trường còn thiếu:"
    btn_text = "Check Answer" if lang == "en" else "Kiểm tra Đáp án"
    hint_label = "Quick hints:" if lang == "en" else "Gợi ý nhanh:"
    placeholder = "Fill in ____" if lang == "en" else "Điền vào chỗ ____"
    hint_chips = "".join(
        f'<button type="button" class="chal-hint-chip" data-fill="{esc(h)}"><code>{esc(h)}</code></button>'
        for h in hints
    )
    return f"""<div class="challenge-box" id="chal-{esc(cid)}" data-answers="{esc('|'.join(expected_answers))}" data-explain="{esc(explain_ok)}">
  <div class="chal-head">
    <span class="chal-badge">{esc(badge)}</span>
    <h3>{inline_code(title)}</h3>
  </div>
  <div class="chal-prompt">{prompt_html}</div>
  <pre class="chal-code"><code>{esc(code_template.strip())}</code></pre>
  <div class="chal-input-row">
    <label for="chal-input-{esc(cid)}">{esc(lbl_text)}</label>
    <input id="chal-input-{esc(cid)}" type="text" class="chal-input" placeholder="{esc(placeholder)}" autocomplete="off" spellcheck="false" />
    <button type="button" class="btn btn-primary chal-check-btn">{esc(btn_text)}</button>
  </div>
  <div class="chal-hints"><span>{esc(hint_label)}</span> {hint_chips}</div>
  <p class="chal-feedback" role="status" aria-live="polite"></p>
</div>"""


def practice_check(
    qid: str,
    question: str,
    options: list[str],
    correct_idx: int,
    explain: str,
    lang: str = "en",
) -> str:
    """Render an Odyssey-style inline lesson multiple-choice Practice Check."""
    badge = "Knowledge Check" if lang == "en" else "Kiểm tra Nhanh"
    opts_html = "".join(
        f'<button type="button" class="practice-opt" data-idx="{idx}">{inline_code(opt)}</button>'
        for idx, opt in enumerate(options)
    )
    return f"""<div class="practice-box" id="prac-{esc(qid)}" data-correct="{correct_idx}" data-explain="{esc(explain)}">
  <span class="chal-badge">{esc(badge)}</span>
  <h3>{inline_code(question)}</h3>
  <div class="practice-options">{opts_html}</div>
  <p class="practice-feedback" role="status" aria-live="polite"></p>
</div>"""


def lesson_footer(lesson_key: str, takeaways: list[str], lang: str = "en") -> str:
    """Render Key Takeaways + persistent 'Mark Lesson Complete' button."""
    heading = "Key Takeaways" if lang == "en" else "Điểm Cốt lõi Cần Nhớ"
    btn_label = "Mark Lesson Complete" if lang == "en" else "Đánh dấu Hoàn thành Bài học"
    items = "".join(f"<li>{inline_code(t)}</li>" for t in takeaways)
    return f"""<div class="takeaways-box">
  <div class="takeaways-head">
    <h3>{esc(heading)}</h3>
    <button type="button" class="btn lesson-complete-btn" data-lesson-key="{esc(lesson_key)}" aria-pressed="false">{esc(btn_label)}</button>
  </div>
  <ul>{items}</ul>
</div>"""
