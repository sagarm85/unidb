#!/usr/bin/env python3
"""Render a benchmark report Markdown file to a self-contained, styled HTML page.

This is a *presentation layer* over the Markdown the bench already emits
(`scripts/report.sh` / `benches/decompose.rs`). The Markdown stays the source
of truth (git-diffable, greppable, consumed by `compare_bench.py`); this script
produces a `report_*.html` sibling that is nicer to read and share.

Design goals:
  * Pure stdlib — no pip deps (matches `compare_bench.py`, `mm_resource_report.py`).
  * Content-agnostic — it parses whatever tables are present, so relabelled
    columns or new tables render without code changes. All emphasis (winner
    pills, ratio coloring, PASS/FAIL) is pattern-based on cell *content*, never
    on a fixed column index, so it degrades gracefully.
  * Self-contained + theme-aware — one inlined <style>, no external assets.

Usage:
    render_report.py <input.md> [output.html]
    # default output = <input>.html
"""

from __future__ import annotations

import html
import re
import sys
from pathlib import Path

# ── inline styling (token-based, light + dark) ───────────────────────────────
STYLE = """
:root{
  --ground:#fbfcfd;--surface:#f1f5f6;--surface-2:#e8eef0;--ink:#14202a;--ink-soft:#46545f;
  --hair:#d5dee1;--hair-strong:#bfcbd0;--accent:#0f7a75;--accent-soft:#d3ebe9;
  --win:#12704a;--win-soft:#d9efe3;--win-line:#63b891;--lose:#9a4a2a;--lose-soft:#f2e0d6;
  --lose-line:#d69a7a;--flag:#8a5a00;--flag-soft:#f2e6cc;--flag-line:#d8b46a;
  --mono:ui-monospace,"SF Mono","SFMono-Regular","Menlo","Cascadia Mono","Roboto Mono",monospace;
  --sans:ui-sans-serif,system-ui,-apple-system,"Segoe UI","Helvetica Neue",sans-serif;
}
@media (prefers-color-scheme:dark){:root{
  --ground:#0d1317;--surface:#151e24;--surface-2:#1b262d;--ink:#e4edf1;--ink-soft:#96a6af;
  --hair:#24323a;--hair-strong:#33454e;--accent:#4bc6bd;--accent-soft:#123430;
  --win:#5bd39a;--win-soft:#10312244;--win-line:#2f7a58;--lose:#e29b76;--lose-soft:#33201644;
  --lose-line:#7a4a2f;--flag:#e0b463;--flag-soft:#3a2e1444;--flag-line:#7a5f2a;
}}
:root[data-theme="light"]{
  --ground:#fbfcfd;--surface:#f1f5f6;--surface-2:#e8eef0;--ink:#14202a;--ink-soft:#46545f;
  --hair:#d5dee1;--hair-strong:#bfcbd0;--accent:#0f7a75;--accent-soft:#d3ebe9;
  --win:#12704a;--win-soft:#d9efe3;--win-line:#63b891;--lose:#9a4a2a;--lose-soft:#f2e0d6;
  --lose-line:#d69a7a;--flag:#8a5a00;--flag-soft:#f2e6cc;--flag-line:#d8b46a;
}
:root[data-theme="dark"]{
  --ground:#0d1317;--surface:#151e24;--surface-2:#1b262d;--ink:#e4edf1;--ink-soft:#96a6af;
  --hair:#24323a;--hair-strong:#33454e;--accent:#4bc6bd;--accent-soft:#123430;
  --win:#5bd39a;--win-soft:#10312244;--win-line:#2f7a58;--lose:#e29b76;--lose-soft:#33201644;
  --lose-line:#7a4a2f;--flag:#e0b463;--flag-soft:#3a2e1444;--flag-line:#7a5f2a;
}
*{box-sizing:border-box;}
body{margin:0;background:var(--ground);color:var(--ink);font-family:var(--sans);line-height:1.55;-webkit-font-smoothing:antialiased;}
.wrap{max-width:980px;margin:0 auto;padding:40px 24px 80px;}
h1{font-size:clamp(25px,4vw,36px);line-height:1.14;margin:0 0 18px;letter-spacing:-.02em;text-wrap:balance;font-weight:640;}
h2{font-size:20px;margin:44px 0 4px;letter-spacing:-.01em;font-weight:620;border-bottom:1px solid var(--hair);padding-bottom:8px;}
h3{font-size:15.5px;margin:28px 0 4px;font-weight:620;color:var(--ink);}
p{font-size:14.5px;color:var(--ink);max-width:74ch;}
p.muted,em{color:var(--ink-soft);}
hr{border:none;border-top:1px solid var(--hair);margin:34px 0;}
a{color:var(--accent);}
code{font-family:var(--mono);font-size:.9em;background:var(--surface-2);padding:1px 5px;border-radius:4px;}
blockquote{margin:16px 0;padding:12px 16px;border-left:3px solid var(--accent);background:var(--surface);border-radius:0 8px 8px 0;font-size:13.5px;color:var(--ink-soft);}
blockquote p{margin:4px 0;color:inherit;font-size:inherit;}
ul{padding-left:20px;} li{font-size:14px;margin:4px 0;}
.meta{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:1px;background:var(--hair);border:1px solid var(--hair);border-radius:9px;overflow:hidden;margin:22px 0;}
.meta div{background:var(--surface);padding:10px 13px;}
.meta dt{font-family:var(--mono);font-size:10.5px;letter-spacing:.07em;text-transform:uppercase;color:var(--ink-soft);margin:0 0 3px;}
.meta dd{margin:0;font-family:var(--mono);font-size:13px;font-variant-numeric:tabular-nums;overflow-wrap:anywhere;}
.scroll{overflow-x:auto;border:1px solid var(--hair);border-radius:9px;background:var(--surface);margin:16px 0;}
table{border-collapse:collapse;width:100%;font-family:var(--mono);font-size:12.5px;}
th,td{padding:8px 12px;text-align:right;white-space:nowrap;border-bottom:1px solid var(--hair);font-variant-numeric:tabular-nums;}
th{color:var(--ink-soft);font-weight:600;background:var(--surface-2);}
td.l,th.l{text-align:left;white-space:normal;}
tr:last-child td{border-bottom:none;}
.win-c{color:var(--win);} .lose-c{color:var(--lose);}
.pill{display:inline-block;font-size:11px;padding:2px 8px;border-radius:20px;font-family:var(--mono);}
.pill.win{background:var(--win-soft);color:var(--win);}
.pill.lose{background:var(--lose-soft);color:var(--lose);}
.pill.pass{background:var(--win-soft);color:var(--win);}
.pill.fail{background:var(--lose-soft);color:var(--lose);}
footer{margin-top:56px;padding-top:20px;border-top:1px solid var(--hair);font-size:12px;color:var(--ink-soft);}
"""

THEME_JS = ""  # theme follows the viewer; no script needed.

# ── inline markdown (bold / code / italic) ───────────────────────────────────
_CODE = re.compile(r"`([^`]+)`")
_BOLD = re.compile(r"\*\*(.+?)\*\*")
# italic: _text_ only when underscores are not glued to word characters, so
# snake_case identifiers (usually inside backticks anyway) are left alone.
_ITAL = re.compile(r"(?<![\w`])_([^_`]+?)_(?![\w])")


def inline(text: str) -> str:
    """Escape HTML, then apply the small inline-markdown subset the reports use."""
    # Protect code spans first: capture raw, escape, drop a placeholder.
    spans: list[str] = []

    def _stash(m: re.Match) -> str:
        spans.append(html.escape(m.group(1)))
        return f"\x00{len(spans) - 1}\x00"

    text = _CODE.sub(_stash, text)
    text = html.escape(text)
    text = _BOLD.sub(r"<strong>\1</strong>", text)
    text = _ITAL.sub(r"<em>\1</em>", text)
    # restore code spans
    text = re.sub(r"\x00(\d+)\x00", lambda m: f"<code>{spans[int(m.group(1))]}</code>", text)
    return text


# ── cell emphasis (pattern-based; content-agnostic) ──────────────────────────
_RATIO = re.compile(r"^[−-]?\d+(?:\.\d+)?×$")  # e.g. 1.19× / 0.57× / 49.64×
_WINNER = re.compile(r"^(unidb|postgres|pg)\b", re.IGNORECASE)


def cell_html(raw: str, is_first: bool) -> str:
    """Render one table cell, adding presentation-only emphasis by content."""
    stripped = raw.strip()
    plain = re.sub(r"[*`_]", "", stripped)  # emphasis-free view for matching

    # ratio like 1.19× → green if >=1, red if <1 (skip signed deltas w/o ×)
    if _RATIO.match(plain):
        num = plain.rstrip("×").replace("−", "-")
        try:
            cls = "win-c" if float(num) >= 1.0 else "lose-c"
            return f'<td class="{cls}">{inline(stripped)}</td>'
        except ValueError:
            pass

    # PASS / FAIL / HANG
    up = plain.upper()
    if up == "PASS":
        return '<td><span class="pill pass">PASS</span></td>'
    if up in ("FAIL", "HANG", "TIMEOUT"):
        return f'<td><span class="pill fail">{html.escape(plain)}</span></td>'

    # winner remark ("unidb +19%", "postgres +76%") → pill
    if _WINNER.match(plain) and "%" in plain:
        low = plain.lower()
        cls = "win" if low.startswith("unidb") else "lose"
        return f'<td class="l"><span class="pill {cls}">{inline(stripped)}</span></td>'

    align = " l" if (is_first or not _looks_numeric(plain)) else ""
    return f'<td class="{align.strip()}">{inline(stripped)}</td>' if align else f"<td>{inline(stripped)}</td>"


def _looks_numeric(s: str) -> bool:
    return bool(re.match(r"^[−+\-]?[\d,]+(?:\.\d+)?[×%]?$", s)) or s in ("—", "-", "")


# ── block parsing ────────────────────────────────────────────────────────────
def split_row(line: str) -> list[str]:
    line = line.strip()
    if line.startswith("|"):
        line = line[1:]
    if line.endswith("|"):
        line = line[:-1]
    return [c.strip() for c in line.split("|")]


def is_sep_row(cells: list[str]) -> bool:
    return all(re.fullmatch(r":?-{2,}:?", c.strip()) for c in cells if c.strip() != "") and any(cells)


def render_table(header: list[str], rows: list[list[str]]) -> str:
    # metadata table: blank header + 2 cols → definition grid
    if len(header) == 2 and all(h.strip() == "" for h in header):
        cells = "".join(
            f"<div><dt>{inline(r[0])}</dt><dd>{inline(r[1])}</dd></div>"
            for r in rows
            if len(r) == 2
        )
        return f'<dl class="meta">{cells}</dl>'

    thead = "".join(
        f'<th class="l">{inline(h)}</th>' if (i == 0 or not _looks_numeric(re.sub(r"[*`_]", "", h)))
        else f"<th>{inline(h)}</th>"
        for i, h in enumerate(header)
    )
    body = []
    for r in rows:
        # pad/truncate to header width
        r = (r + [""] * len(header))[: len(header)]
        tds = "".join(cell_html(c, is_first=(i == 0)) for i, c in enumerate(r))
        body.append(f"<tr>{tds}</tr>")
    return (
        '<div class="scroll"><table><thead><tr>'
        + thead
        + "</tr></thead><tbody>"
        + "".join(body)
        + "</tbody></table></div>"
    )


def render(md: str) -> tuple[str, str]:
    lines = md.splitlines()
    out: list[str] = []
    title = "Benchmark report"
    i = 0
    n = len(lines)
    while i < n:
        line = lines[i]
        stripped = line.strip()

        if stripped == "":
            i += 1
            continue

        # horizontal rule
        if re.fullmatch(r"-{3,}", stripped):
            out.append("<hr>")
            i += 1
            continue

        # headings
        m = re.match(r"^(#{1,4})\s+(.*)$", stripped)
        if m:
            level = len(m.group(1))
            text = m.group(2)
            if level == 1 and title == "Benchmark report":
                title = re.sub(r"[*`_]", "", text)
            tag = f"h{min(level, 3)}"
            out.append(f"<{tag}>{inline(text)}</{tag}>")
            i += 1
            continue

        # table: a line with '|' followed by a separator row
        if "|" in line and i + 1 < n and is_sep_row(split_row(lines[i + 1])):
            header = split_row(line)
            i += 2
            rows = []
            while i < n and "|" in lines[i] and lines[i].strip():
                rows.append(split_row(lines[i]))
                i += 1
            out.append(render_table(header, rows))
            continue

        # blockquote (may span multiple lines)
        if stripped.startswith(">"):
            buf = []
            while i < n and lines[i].strip().startswith(">"):
                buf.append(lines[i].strip()[1:].strip())
                i += 1
            inner = " ".join(b for b in buf if b)
            out.append(f"<blockquote><p>{inline(inner)}</p></blockquote>")
            continue

        # unordered list
        if re.match(r"^[-*]\s+", stripped):
            items = []
            while i < n and re.match(r"^[-*]\s+", lines[i].strip()):
                items.append(f"<li>{inline(re.sub(r'^[-*]\\s+', '', lines[i].strip()))}</li>")
                i += 1
            out.append("<ul>" + "".join(items) + "</ul>")
            continue

        # paragraph (gather until blank / block boundary)
        buf = [stripped]
        i += 1
        while i < n:
            nxt = lines[i].strip()
            if nxt == "" or nxt.startswith(("#", ">", "|")) or re.match(r"^[-*]\s+", nxt) or re.fullmatch(r"-{3,}", nxt):
                break
            buf.append(nxt)
            i += 1
        cls = ' class="muted"' if buf[0].startswith(("_", "*(")) else ""
        out.append(f"<p{cls}>{inline(' '.join(buf))}</p>")

    body = "\n".join(out)
    page = (
        "<title>" + html.escape(title) + "</title>\n"
        "<meta name='viewport' content='width=device-width, initial-scale=1'>\n"
        "<style>" + STYLE + "</style>\n"
        '<div class="wrap">\n' + body + "\n"
        "<footer>Rendered from the report Markdown by <code>scripts/render_report.py</code> — "
        "presentation only; every number is verbatim from the source. The Markdown remains the source of truth.</footer>\n"
        "</div>"
    )
    return title, page


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__)
        return 2
    src = Path(argv[1])
    if not src.is_file():
        print(f"[render_report] not found: {src}", file=sys.stderr)
        return 1
    dst = Path(argv[2]) if len(argv) > 2 else src.with_suffix(".html")
    _title, page = render(src.read_text(encoding="utf-8"))
    dst.write_text(page, encoding="utf-8")
    print(str(dst))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
