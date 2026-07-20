# unidb — Design & Architecture (engineering reference) — context & regeneration

Provenance and regeneration notes for
[`unidb_design_architecture.html`](unidb_design_architecture.html) and the
generated `unidb_design_architecture.pdf`. Kept out of the guide itself so the
guide reads cleanly. Keep this file current when you edit the guide.

## What the guide is

The **engineer-facing** Design & Architecture reference — the internal companion
to the end-user `unidb_engine_architecture.pdf`. Where the end-user guide is a
product/how-to document that deliberately omits internal identifiers, this one is
for **engine engineers and technical evaluators auditing the internals**, so it
*keeps* them: locked-decision codes (`D1`–`D13`), WAL record-type names, tuple
header fields, and backlog item numbers.

It is a diagram-led **distillation** of the `processing-engines/` collection —
not a replacement. The twelve `processing-engines/*.md` files remain the deep,
per-subsystem source of truth (full record-type tables, the complete crash
matrix, border-case tables, every data structure). This guide is the polished
narrative + hero visuals + key reference tables, sized for a shareable ~9-page
PDF.

## The four hero diagrams (hand-drawn inline SVG)

Unlike the mermaid diagrams in the markdown docs, these are hand-authored SVG so
they render identically in the PDF and match the house visual language (layered
tinted bands, white cards with colored accent bars, labeled flow arrows):

1. **System architecture** — the five-layer stack (Clients → Query & Execution →
   Data Models → Transaction & Concurrency → Storage) with the WAL-before-page
   (D5) write path.
2. **One commit, four models** — the write-path sequence: row + index + vector
   posting + event, then one group-committed fsync.
3. **Recovery flow** — read → analyze → redo → undo → flush, in three phase
   columns.
4. **MVCC visibility** — snapshot definition, backward version chain,
   `is_visible` rule, and the three isolation levels.

If you change an SVG, keep the `viewBox` and the `.figure svg { max-width:100% }`
rule so it scales to the A4 column; test-render before committing.

## Source material (what the content is distilled from)

- `docs/design/processing-engines/*.md` — the primary source (this guide is its
  distillation). When the two disagree, the processing-engines docs (and behind
  them the code) win; refresh this guide.
- `CLAUDE.md` — locked design decisions (`D1`–`D13`) and charter.
- `MEMORY.md` / `PROGRESS.md` — current state and the measured performance
  numbers used in §7. Numbers are never invented; they come from shipped runs.

When the guide disagrees with `CLAUDE.md`/`PROGRESS.md`/the processing-engines
docs, those win.

## Coverage snapshot

Reflects the engine at `FORMAT_VERSION` 11 · 19 WAL record types · 51 crash
points · items 71–99 incorporated. §7's performance table reflects the
items-75–84 Docker report (DELETE selected 0.81×, UPDATE HOT 0.62×, plus the
COUNT/DELETE-all/GROUP-BY wins). Refresh §7 whenever a new Docker report lands
and re-check the FORMAT_VERSION / record-count / crash-point counts in the
title meta line against `format.rs` and `tests/crash/main.rs`.

## Regeneration (produces page-numbered PDF)

Identical toolchain to the end-user guide — headless Google Chrome over the
DevTools protocol (for the page-number footer), Node ≥ 22, no npm install:

```bash
cd docs/design
node render_pdf.mjs unidb_design_architecture.html unidb_design_architecture.pdf
```

Workflow: **edit the HTML, then re-run the command.** The first page is the
title + clickable contents (no cover); every content page carries a footer page
number.

## Mermaid theme note (the other half of this docs pass)

The `processing-engines/*.md` and `how_unidb_stores_data.md` mermaid blocks all
carry a shared `%%{init: {"theme":"base","themeVariables":{…}}}%%` directive
(injected 2026-07-20) so GitHub renders them with one consistent palette
(light-blue nodes, blue/green/amber accents, `Segoe UI`) that matches this
guide's hero SVGs. When adding a new mermaid diagram, prepend the same directive
as its first line so the collection stays visually cohesive.
