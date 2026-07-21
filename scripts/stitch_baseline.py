#!/usr/bin/env python3
"""
stitch_baseline.py — carry skipped bench tables forward from a baseline report.

Usage:
  scripts/stitch_baseline.py <new_report.md> <baseline_report.md>

For every `## Table N — …` section in <new_report> whose body contains a
`_Skipped:` marker (emitted by the decompose bench when a table-selection knob
skipped it), the same-numbered section from <baseline_report> is copied in,
prefixed with an explicit provenance stamp so a carried-forward number can
never be mistaken for a fresh measurement:

  > **Carried forward — NOT re-measured in this run.** Copied from
  > `<baseline file>` (commit `<c>`, dated <d>). …

Honesty rules (§6 CLAUDE.md):
  * A baseline section that is itself skipped is never copied (warned, left
    as-is) — you cannot stitch a hole with a hole.
  * A baseline section that is itself carried forward IS copied unchanged —
    its ORIGINAL provenance stamp is preserved (no re-stamping), and a
    chaining warning is printed. Prefer stitching from a true full run.
  * compare_bench.py excludes carried-forward sections from delta tables.

Exit code 0 always (informational post-processing; report.sh continues).
"""

import os
import re
import sys

MARKER_SKIPPED = "_Skipped:"
MARKER_CARRIED = "Carried forward — NOT re-measured in this run"

# `## Table 1 — …`, `## Table 3.1 — …` etc. Captures the table number.
HEADING_RE = re.compile(r"^## Table ([\d.]+)\b")


def split_sections(text: str):
    """Split markdown into (preamble, [(table_id_or_None, heading, body)]).

    Sections are delimited by `## ` headings; `###` stays inside its parent.
    """
    lines = text.split("\n")
    preamble: list[str] = []
    sections: list[tuple[str | None, str, list[str]]] = []
    cur: list[str] | None = None
    for line in lines:
        if line.startswith("## "):
            m = HEADING_RE.match(line)
            table_id = m.group(1) if m else None
            cur = []
            sections.append((table_id, line, cur))
        elif cur is not None:
            cur.append(line)
        else:
            preamble.append(line)
    return preamble, sections


def header_fact(text: str, key: str) -> str:
    """Pull `| Key | value |` from the report's header table."""
    m = re.search(rf"^\| {re.escape(key)} \| (.+?) \|$", text, re.MULTILINE)
    return m.group(1).strip() if m else "?"


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: stitch_baseline.py <new_report.md> <baseline.md>", file=sys.stderr)
        return 0
    new_path, base_path = sys.argv[1], sys.argv[2]
    if not os.path.isfile(base_path):
        print(f"[stitch] WARNING: baseline not found: {base_path} — nothing stitched.", file=sys.stderr)
        return 0

    with open(new_path, encoding="utf-8") as f:
        new_text = f.read()
    with open(base_path, encoding="utf-8") as f:
        base_text = f.read()

    base_commit = header_fact(base_text, "Commit")
    base_date = header_fact(base_text, "Date")
    base_name = os.path.basename(base_path)

    _, base_sections = split_sections(base_text)
    base_by_id = {tid: (heading, body) for tid, heading, body in base_sections if tid}

    preamble, sections = split_sections(new_text)
    stitched: list[str] = []
    chained: list[str] = []

    out_lines = list(preamble)
    for tid, heading, body in sections:
        body_text = "\n".join(body)
        if tid and MARKER_SKIPPED in body_text and MARKER_CARRIED not in body_text:
            if tid not in base_by_id:
                print(f"[stitch] WARNING: Table {tid} skipped but absent from baseline — left as-is.",
                      file=sys.stderr)
                out_lines.extend([heading, *body])
                continue
            b_heading, b_body = base_by_id[tid]
            b_text = "\n".join(b_body)
            if MARKER_SKIPPED in b_text and MARKER_CARRIED not in b_text:
                print(f"[stitch] WARNING: Table {tid} is skipped in the baseline too — left as-is. "
                      f"Stitch from a FULL baseline run.", file=sys.stderr)
                out_lines.extend([heading, *body])
                continue
            if MARKER_CARRIED in b_text:
                # Chained carry-forward: keep the ORIGINAL stamp untouched.
                chained.append(tid)
                out_lines.extend([b_heading, *b_body])
            else:
                stamp = [
                    "",
                    f"> **Carried forward — NOT re-measured in this run.** Copied from",
                    f"> `{base_name}` (commit {base_commit}, dated {base_date}) because a",
                    f"> table-selection knob skipped this table. Treat as stale if any shared",
                    f"> layer (WAL, commit path, buffer pool, heap, page format) changed since",
                    f"> that commit — re-run the full bench in that case.",
                ]
                # Drop the baseline's leading blank line to keep spacing tidy.
                out_lines.extend([b_heading, *stamp, *b_body])
            stitched.append(tid)
        else:
            out_lines.extend([heading, *body])

    if not stitched:
        print("[stitch] no skipped tables found — report left unchanged.", file=sys.stderr)
        return 0

    with open(new_path, "w", encoding="utf-8") as f:
        f.write("\n".join(out_lines))

    print(f"[stitch] carried forward Table(s) {', '.join(stitched)} from {base_name} "
          f"(commit {base_commit}, dated {base_date}).", file=sys.stderr)
    if chained:
        print(f"[stitch] WARNING: Table(s) {', '.join(chained)} were ALREADY carried forward in the "
              f"baseline — original provenance preserved. Prefer stitching from a full run.",
              file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
