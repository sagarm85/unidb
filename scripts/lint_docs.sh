#!/usr/bin/env bash
# lint_docs.sh — guards for the MEMORY/PROGRESS working-set + archive split
# (introduced 2026-07-22; policy in CLAUDE.md §0.4).
#
# Checks:
#   1. Size thresholds: MEMORY.md ≤ ~50 KB, PROGRESS.md ≤ ~120 KB. Crossing one
#      means it's time to roll older entries into docs/history/ — the whole
#      point is that every session's mandatory read stays small.
#   2. Cross-reference integrity: every `PROGRESS.md "<name>"` reference in the
#      repo's docs must resolve — the quoted name must appear in PROGRESS.md or
#      its archive. Archives keep headings verbatim precisely for this.
#   3. Archive pointers: MEMORY.md and PROGRESS.md must each contain a pointer
#      to their docs/history/ archive (so a reader knows the tail exists).
#
# Run from repo root:  ./scripts/lint_docs.sh   (exit 1 on any finding)

set -euo pipefail
cd "$(dirname "$0")/.."

fail=0

# --- 1. size thresholds -------------------------------------------------------
mem_kb=$(( $(wc -c < MEMORY.md) / 1024 ))
prog_kb=$(( $(wc -c < PROGRESS.md) / 1024 ))
if [ "$mem_kb" -gt 50 ]; then
  echo "SIZE: MEMORY.md is ${mem_kb} KB (> 50 KB) — roll entries older than the last ~5 sessions into docs/history/ (CLAUDE.md §0.4)"
  fail=1
fi
if [ "$prog_kb" -gt 120 ]; then
  echo "SIZE: PROGRESS.md is ${prog_kb} KB (> 120 KB) — roll older ledger entries into docs/history/ and refresh the entry index"
  fail=1
fi

# --- 2. PROGRESS.md cross-references resolve ---------------------------------
# Collect every quoted reference of the form: PROGRESS.md "Some Entry Name".
# The separator allows only backtick/colon/comma/space — a period or other
# sentence punctuation between `PROGRESS.md` and the quote is prose, not a
# reference. Template placeholders (containing < or >) are skipped.
refs=$(grep -rhoE 'PROGRESS\.md`?[:,]? ?"[^"]+"' \
         MEMORY.md README.md CLAUDE.md docs/ 2>/dev/null \
       | sed -E 's/.*"([^"]+)".*/\1/' | sort -u)

while IFS= read -r ref; do
  [ -z "$ref" ] && continue
  case "$ref" in *"<"*|*">"*) continue ;; esac
  if ! grep -qF "$ref" PROGRESS.md docs/history/PROGRESS_ARCHIVE_*.md 2>/dev/null; then
    echo "DANGLING-REF: PROGRESS.md \"$ref\" — not found in PROGRESS.md or docs/history/PROGRESS_ARCHIVE_*.md"
    fail=1
  fi
done <<< "$refs"

# --- 3. archive pointers present ---------------------------------------------
grep -q "docs/history/MEMORY_ARCHIVE" MEMORY.md || {
  echo "POINTER: MEMORY.md has no pointer to its docs/history/ archive"; fail=1; }
grep -q "docs/history/PROGRESS_ARCHIVE" PROGRESS.md || {
  echo "POINTER: PROGRESS.md has no pointer to its docs/history/ archive"; fail=1; }

if [ "$fail" -eq 0 ]; then
  echo "docs lint: OK (MEMORY.md ${mem_kb} KB, PROGRESS.md ${prog_kb} KB, $(echo "$refs" | grep -c . ) cross-refs resolve)"
fi
exit "$fail"
