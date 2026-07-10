#!/usr/bin/env python3
"""
mm_resource_report.py — correlate bench phase windows with docker-stats samples
and append a per-phase CPU/memory table to the multi-model report.

Inputs (all produced by scripts/docker_report.sh + the decompose mmreport bench):
  report.md   the report to append to
  phases.csv  lines: <phase_name>,<start|end>,<unix_ms>   (written by the bench)
  stats.csv   lines: <unix_ms>,<container>,<cpu%>,<mem_used / mem_limit>  (host sampler)

For each phase window, for each container role (unidb bench vs postgres), it
reports peak/avg CPU% and peak memory over the samples that fall inside the
window. The embedded-vs-server asymmetry is stated explicitly — this is NOT a
clean head-to-head number.

Usage: mm_resource_report.py <report.md> <phases.csv> <stats.csv>
"""
from __future__ import annotations

import sys
from pathlib import Path


def parse_mem_to_bytes(s: str) -> float:
    """'24.03MiB' / '1.2GiB' / '512KiB' / '900B' -> bytes."""
    s = s.strip()
    units = {"B": 1, "KIB": 1024, "MIB": 1024**2, "GIB": 1024**3, "TIB": 1024**4,
             "KB": 1000, "MB": 1000**2, "GB": 1000**3}
    num = ""
    for i, ch in enumerate(s):
        if ch.isdigit() or ch == ".":
            num += ch
        else:
            unit = s[i:].strip().upper()
            try:
                return float(num) * units.get(unit, 1)
            except ValueError:
                return 0.0
    try:
        return float(num)
    except ValueError:
        return 0.0


def role_of(container: str) -> str | None:
    # NB: check "postgres" FIRST — the compose project is "unidb-fair-bench", so
    # BOTH container names contain "bench" (…-bench-1 and …-postgres-1). Matching
    # "bench" first would misclassify the postgres container as unidb.
    c = container.lower()
    if "postgres" in c:
        return "postgres"
    if "bench" in c:
        return "unidb"
    return None


def fmt_mem(b: float) -> str:
    if b <= 0:
        return "—"
    for unit, div in (("GiB", 1024**3), ("MiB", 1024**2), ("KiB", 1024)):
        if b >= div:
            return f"{b/div:.1f} {unit}"
    return f"{b:.0f} B"


def main() -> int:
    if len(sys.argv) != 4:
        sys.stderr.write("usage: mm_resource_report.py <report.md> <phases.csv> <stats.csv>\n")
        return 2
    report, phases_p, stats_p = (Path(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3]))

    # ---- phase windows ----
    starts: dict[str, int] = {}
    windows: dict[str, tuple[int, int]] = {}
    order: list[str] = []
    if phases_p.exists():
        for line in phases_p.read_text().splitlines():
            parts = line.split(",")
            if len(parts) != 3:
                continue
            name, edge, ms = parts[0], parts[1], parts[2]
            try:
                ms_i = int(ms)
            except ValueError:
                continue
            if edge == "start":
                starts[name] = ms_i
                if name not in order:
                    order.append(name)
            elif edge == "end" and name in starts:
                windows[name] = (starts[name], ms_i)

    # ---- stats samples ----
    samples: list[tuple[int, str, float, float]] = []  # (ms, role, cpu%, mem_bytes)
    if stats_p.exists():
        for line in stats_p.read_text().splitlines():
            parts = line.split(",")
            if len(parts) < 4:
                continue
            try:
                ms_i = int(parts[0])
            except ValueError:
                continue
            role = role_of(parts[1])
            if role is None:
                continue
            cpu = 0.0
            try:
                cpu = float(parts[2].replace("%", "").strip())
            except ValueError:
                pass
            mem = parse_mem_to_bytes(parts[3].split("/")[0])
            samples.append((ms_i, role, cpu, mem))

    def agg(name: str, role: str):
        if name not in windows:
            return None
        lo, hi = windows[name]
        vals = [(c, m) for (ms, r, c, m) in samples if r == role and lo <= ms <= hi]
        if not vals:
            return None
        cpus = [c for c, _ in vals]
        mems = [m for _, m in vals]
        return (max(cpus), sum(cpus) / len(cpus), max(mems), len(vals))

    # ---- build the appended section ----
    lines: list[str] = []
    A = lines.append
    A("")
    A("## CPU / Memory per phase (docker stats)")
    A("")
    A("Per-phase resource use of each **container**, sampled by `docker stats` on the "
      "host (~1 s cadence) and correlated to the bench's phase windows.")
    A("")
    A("> **Read with care — this is not a clean head-to-head.** unidb is *embedded* "
      "(all its work is inside the bench container's process). Postgres is *client-"
      "server*: the bench container only holds the client; the real query work — plus "
      "shared buffers, WAL writer, checkpointer, autovacuum — lives in the **postgres** "
      "container. So \"unidb CPU\" is the engine, while \"postgres CPU\" is the whole "
      "server. Different resource models; compare trends, not absolute parity. Short "
      "phases may have too few 1 s samples to be meaningful (shown as —).")
    A("")
    A("| phase | unidb CPU% (peak/avg) | unidb mem (peak) | postgres CPU% (peak/avg) | postgres mem (peak) | n |")
    A("|-------|----------------------|------------------|--------------------------|---------------------|---|")

    any_row = False
    for name in order:
        u = agg(name, "unidb")
        p = agg(name, "postgres")
        if u is None and p is None:
            continue
        any_row = True
        n = max((u[3] if u else 0), (p[3] if p else 0))
        u_cpu = f"{u[0]:.0f} / {u[1]:.0f}" if u else "—"
        u_mem = fmt_mem(u[2]) if u else "—"
        p_cpu = f"{p[0]:.0f} / {p[1]:.0f}" if p else "—"
        p_mem = fmt_mem(p[2]) if p else "—"
        A(f"| {name} | {u_cpu} | {u_mem} | {p_cpu} | {p_mem} | {n} |")

    if not any_row:
        A("| _(no samples captured — phases too short for the 1 s sampler, or "
          "docker stats unavailable)_ | — | — | — | — | 0 |")

    # Whole-run peaks.
    def run_peak(role: str):
        vals = [(c, m) for (_, r, c, m) in samples if r == role]
        if not vals:
            return None
        return (max(c for c, _ in vals), max(m for _, m in vals))

    A("")
    up, pp = run_peak("unidb"), run_peak("postgres")
    u_txt = f"unidb — CPU {up[0]:.0f}%, mem {fmt_mem(up[1])}" if up else "unidb — n/a"
    p_txt = f"postgres — CPU {pp[0]:.0f}%, mem {fmt_mem(pp[1])}" if pp else "postgres — n/a"
    A(f"**Whole-run peaks:** {u_txt}; {p_txt}.")
    A("")

    with report.open("a") as f:
        f.write("\n".join(lines) + "\n")
    sys.stderr.write(f"[mm_resource_report] appended CPU/mem section to {report.name}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
