#!/usr/bin/env python3
"""
compare_bench.py — compare a new report against the latest benchmark.

Usage:
  scripts/compare_bench.py <new_report> [<benchmark>]

If <benchmark> is omitted, finds the latest benchmark_*.md in docker/out/ or
docs/performance/ under the repo root (derived from this script's location).

Prints a delta table to stdout. Wins are highlighted. No exit-code signalling —
this is informational only; the caller (report.sh) always continues.
"""

import sys, re, os, glob

# ── ANSI colours (disabled if not a tty) ─────────────────────────────────────
def _tty():
    return hasattr(sys.stdout, "isatty") and sys.stdout.isatty()

GREEN  = "\033[32m" if _tty() else ""
YELLOW = "\033[33m" if _tty() else ""
RED    = "\033[31m" if _tty() else ""
BOLD   = "\033[1m"  if _tty() else ""
RESET  = "\033[0m"  if _tty() else ""

# ── metric parsing ────────────────────────────────────────────────────────────
_RATIO_RE = re.compile(r'([\d.]+)×')

def _ratio(cell: str):
    m = _RATIO_RE.search(cell.strip())
    return float(m.group(1)) if m else None

def _cols(line: str):
    """Split a markdown table row into stripped cell strings."""
    parts = line.strip().strip('|').split('|')
    return [p.strip() for p in parts]

# Provenance stamp written by scripts/stitch_baseline.py — sections carrying it
# hold numbers copied from an OLDER report, so they must never enter the delta
# comparison (they would diff a baseline against itself, or worse, against a
# different baseline, and read as a fake regression/win).
_CARRIED_MARKER = "Carried forward — NOT re-measured in this run"

_HEADING_RE = re.compile(r'^## Table ([\d.]+)\b')


def parse_metrics(path: str) -> dict:
    """
    Section-aware parse. Returns:
      'crud':   {operation_label: ratio}   — Table 3 rows (unidb÷PG)
      'fk':     {operation_label: ratio}   — Table 5 rows (unidb÷PG)
      'w4w0':   {row_size_str:   ratio}    — W4/W0 column, Table 1 ONLY
      'pg_abs': {operation_label: rec/s}   — Table 3 POSTGRES absolute rec/s
                (environment canary — Postgres code never changes between our
                runs, so a large move in ITS absolutes means the environment
                changed, and cross-run RATIO deltas are not evidence; item 108)
    Sections stitched in by stitch_baseline.py (carried forward from an older
    report) are excluded entirely. Restricting W4/W0 to Table 1 also fixes a
    prior collision where Table 4 rows (same integer-keyed shape, ratio in the
    last column) silently overwrote the W4/W0 entries.
    """
    crud, fk, w4w0, pg_abs = {}, {}, {}, {}
    table_id = None     # "1", "3.1", "4", … for the current ## Table section
    carried = False     # current section was carried forward — ignore its rows

    with open(path, encoding="utf-8") as f:
        for raw in f:
            line = raw.rstrip('\n')

            if line.startswith('## '):
                m = _HEADING_RE.match(line)
                table_id = m.group(1) if m else None
                carried = False
            if _CARRIED_MARKER in line:
                carried = True

            if carried or not line.startswith('|'):
                continue
            cols = _cols(line)
            if len(cols) < 5:
                continue

            # Table 1 W4/W0 row: first col is a plain integer (row size),
            # last col has the W4/W0 ratio.
            if table_id == '1' and re.match(r'^\d+$', cols[0]) and _RATIO_RE.search(cols[-1]):
                w4w0[cols[0]] = _ratio(cols[-1])
                continue

            # Table 3 / Table 5: operation name (text), col[4] is unidb÷PG ratio
            if table_id in ('3', '5') and _ratio(cols[4]) is not None:
                op = cols[0]
                # skip header rows and separator rows
                if re.match(r'^[-:| ]+$', op) or op.lower() in ('operation', 'op'):
                    continue
                ratio = _ratio(cols[4])
                if ratio is None:
                    continue
                if table_id == '5':
                    fk[op] = ratio
                else:
                    crud[op] = ratio
                    # Environment canary: Postgres absolute rec/s (col 3).
                    try:
                        pg_abs[op] = float(cols[3])
                    except (ValueError, IndexError):
                        pass

    return {'crud': crud, 'fk': fk, 'w4w0': w4w0, 'pg_abs': pg_abs}


# ── formatting ────────────────────────────────────────────────────────────────
def _delta_str(old: float, new: float, higher_is_better: bool = True):
    """Return (delta_pct_str, colour)."""
    if old == 0:
        return "  n/a", ""
    pct = (new - old) / old * 100
    improved = pct > 0 if higher_is_better else pct < 0
    sign = "+" if pct >= 0 else ""
    s = f"{sign}{pct:.0f}%"
    colour = GREEN if improved and abs(pct) >= 3 else (RED if not improved and abs(pct) >= 3 else "")
    win = f"  {BOLD}{GREEN}▲ WIN{RESET}" if improved and abs(pct) >= 5 else (
          f"  {RED}▼ LOSS{RESET}" if not improved and abs(pct) >= 5 else "")
    return s, colour, win


def _row(label: str, old: float, new: float, higher_is_better: bool = True, w: int = 36):
    pct, colour, win = _delta_str(old, new, higher_is_better)
    return (f"  {label:<{w}}  {old:>6.2f}×  {new:>6.2f}×  "
            f"{colour}{pct:>6}{RESET}{win}")


def _section(title: str, old_m: dict, new_m: dict, higher_is_better: bool = True):
    keys = list(old_m.keys() | new_m.keys())
    if not keys:
        return []
    lines = [f"\n{BOLD}{title}{RESET}"]
    col_label = "rows" if "W4/W0" in title else "operation"
    lines.append(f"  {col_label:<36}  {'before':>7}  {'after':>6}  {'delta':>6}")
    lines.append("  " + "-" * 70)
    for k in keys:
        o = old_m.get(k)
        n = new_m.get(k)
        if o is None or n is None:
            lines.append(f"  {k:<36}  {'—':>7}  {'—':>6}")
            continue
        lines.append(_row(k, o, n, higher_is_better))
    return lines


# ── main ──────────────────────────────────────────────────────────────────────
def find_latest_benchmark(repo_root: str):
    patterns = [
        os.path.join(repo_root, "docker", "out", "benchmark_*.md"),
        os.path.join(repo_root, "docs", "performance", "benchmark_*.md"),
    ]
    files = []
    for p in patterns:
        files.extend(glob.glob(p))
    if not files:
        return None
    return max(files, key=os.path.getmtime)


def main():
    if len(sys.argv) < 2:
        print("usage: compare_bench.py <new_report> [<benchmark>]", file=sys.stderr)
        sys.exit(0)

    new_path = sys.argv[1]
    repo_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

    bench_path = sys.argv[2] if len(sys.argv) >= 3 else find_latest_benchmark(repo_root)
    if bench_path is None:
        print(f"\n{YELLOW}[compare] No benchmark found — run scripts/promote_bench.sh to set one.{RESET}")
        return

    if os.path.abspath(new_path) == os.path.abspath(bench_path):
        print(f"\n{YELLOW}[compare] New report IS the current benchmark — nothing to compare.{RESET}")
        return

    try:
        old = parse_metrics(bench_path)
        new = parse_metrics(new_path)
    except Exception as e:
        print(f"\n{YELLOW}[compare] Could not parse reports: {e}{RESET}", file=sys.stderr)
        return

    bench_name = os.path.basename(bench_path)
    new_name   = os.path.basename(new_path)

    print(f"\n{BOLD}{'─' * 72}{RESET}")
    print(f"{BOLD}  vs {bench_name}{RESET}  (benchmark → this run)")
    print(f"{BOLD}{'─' * 72}{RESET}")

    # ── Environment canary (item 108) ────────────────────────────────────────
    # Postgres is code-identical across our runs; if its own absolute rec/s
    # moved a lot, the ENVIRONMENT changed (VM fsync mood, CPU contention) and
    # the ratio deltas below mostly measure that, not unidb. The 2026-07-19 vs
    # 2026-07-21 reports are the canonical example: PG absolutes moved 2.1–28×
    # per op and every apparent unidb "regression" dissolved under absolutes.
    drifts = []
    for op, new_v in new.get('pg_abs', {}).items():
        old_v = old.get('pg_abs', {}).get(op)
        if old_v and old_v > 0:
            drifts.append(abs(new_v - old_v) / old_v)
    if drifts:
        drifts.sort()
        median = drifts[len(drifts) // 2]
        if median > 0.25:
            print(f"\n{BOLD}{YELLOW}  ⚠ ENVIRONMENT CHANGED between these runs:{RESET}")
            print(f"{YELLOW}  Postgres's own absolute throughput moved {median * 100:.0f}% (median across "
                  f"{len(drifts)} CRUD ops) with identical PG code.{RESET}")
            print(f"{YELLOW}  Treat the ratio deltas below as environment noise unless a row's "
                  f"unidb ABSOLUTE rec/s also regressed (check the reports directly).{RESET}")

    out = []
    out += _section("Table 3 — CRUD (unidb ÷ PG, higher = unidb closer to PG)",
                    old['crud'], new['crud'], higher_is_better=True)
    out += _section("Table 5 — FK join (unidb ÷ PG, higher = unidb closer to PG)",
                    old['fk'], new['fk'], higher_is_better=True)
    out += _section("W4/W0 — multi-model tax (lower = less overhead per added model)",
                    old['w4w0'], new['w4w0'], higher_is_better=False)

    if not any(l.strip() and not l.startswith(('\n', f"\n{BOLD}")) for l in out):
        print(f"  {YELLOW}No comparable metrics found in both reports.{RESET}")
    else:
        print('\n'.join(out))

    print(f"\n{BOLD}{'─' * 72}{RESET}")
    print(f"  To promote this run: {BOLD}scripts/promote_bench.sh {new_path}{RESET}\n")


if __name__ == "__main__":
    main()
