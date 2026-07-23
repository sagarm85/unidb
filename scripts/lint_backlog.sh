#!/usr/bin/env bash
# lint_backlog.sh — mechanical consistency check for docs/backlog/.
#
# Catches the two drift classes found (twice each) in the 2026-07-22 doc audit:
#   1. Registry drift: an item FILE says SHIPPED but backlog_index.md's row
#      still says NOT STARTED (items 107/109/110/111), or vice versa — a file
#      header never flipped at ship time (items 27/34/45/61/62/66/68/69/74/
#      93/96-99/101).
#   2. Orphans: a numbered file with no registry row, or a duplicate number
#      (the unregistered second `42_…` file, renumbered to 113).
#
# Status vocab equivalences accepted: SHIPPED≈RESOLVED≈FIXED (done),
# NOT STARTED, IN PROGRESS, PARTIAL.
#
# Run from repo root:  ./scripts/lint_backlog.sh   (exit 1 on any finding)

set -euo pipefail
cd "$(dirname "$0")/../docs/backlog"

fail=0

norm() {
  # Map a status word to a lifecycle bucket.
  case "$1" in
    SHIPPED|RESOLVED|FIXED) echo DONE ;;
    *) echo "$1" ;;
  esac
}

seen_nums=""
for f in [0-9]*_*.md; do
  n="${f%%_*}"

  # duplicate-number check
  case " $seen_nums " in
    *" $n "*) echo "DUPLICATE-ID: $f reuses number $n"; fail=1 ;;
  esac
  seen_nums="$seen_nums $n"

  # registry-row check
  row=$(grep -m1 -E "^\| 0*$n \|" backlog_index.md || true)
  if [ -z "$row" ]; then
    echo "ORPHAN: $f has no row in backlog_index.md"
    fail=1
    continue
  fi

  # Status header: standard `**Status:**` line, or a `| **Status** | … |` table row (e.g. item 32).
  fs=$(grep -m1 -E "^(\*\*Status|\| \*\*Status\*\*)" "$f" | grep -oE "SHIPPED|NOT STARTED|IN PROGRESS|RESOLVED|FIXED|PARTIAL|INVESTIGATION COMPLETE" | head -1 || true)
  is=$(echo "$row" | grep -oE "SHIPPED|NOT STARTED|IN PROGRESS|RESOLVED|FIXED|PARTIAL|INVESTIGATION COMPLETE" | head -1 || true)
  [ -z "$fs" ] && { echo "NO-STATUS-HEADER: $f (add a **Status:** line per CONVENTIONS.md)"; fail=1; continue; }
  [ -z "$is" ] && continue  # row with unusual wording — human judgment

  if [ "$(norm "$fs")" != "$(norm "$is")" ]; then
    echo "MISMATCH: $f file=$fs index=$is"
    fail=1
  fi
done

if [ "$fail" -eq 0 ]; then
  echo "backlog lint: OK ($(ls [0-9]*_*.md | wc -l | tr -d ' ') numbered files checked)"
fi
exit "$fail"
