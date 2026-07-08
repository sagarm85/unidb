//! P4.b `ORDER BY` execution: an in-memory sort that falls back to an external
//! merge sort when the input exceeds a row budget, so a large `ORDER BY` never
//! holds the whole result in memory at once. Sorted runs are spilled to temp
//! files and merged with a streaming k-way heap (one row resident per run).
//!
//! NULL ordering matches SQLite (NULL is the smallest value) so the differential
//! tests agree: ascending puts NULLs first, descending last.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;

use crate::error::{DbError, Result};
use crate::sql::executor::literal_ord;
use crate::sql::logical::Literal;
use crate::sql::plan::SortKey;

/// The in-memory row budget before `ORDER BY` spills to an external merge sort.
/// Overridable via `UNIDB_SORT_MEM_ROWS` (tests force spill with a small value).
pub fn sort_mem_rows() -> usize {
    std::env::var("UNIDB_SORT_MEM_ROWS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(1_000_000)
}

/// Compare two rows by the ordered `keys`. NULL sorts smallest; direction is
/// applied per key.
fn cmp_rows(a: &[Literal], b: &[Literal], keys: &[SortKey]) -> Ordering {
    for k in keys {
        let (av, bv) = (&a[k.column], &b[k.column]);
        let base = match (matches!(av, Literal::Null), matches!(bv, Literal::Null)) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less, // NULL is smallest
            (false, true) => Ordering::Greater,
            (false, false) => literal_ord(av, bv).unwrap_or(Ordering::Equal),
        };
        let ord = if k.asc { base } else { base.reverse() };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// Sort `rows` by `keys`, using an external merge sort when `rows` exceeds
/// `mem_rows`.
pub fn sort_rows(
    mut rows: Vec<Vec<Literal>>,
    keys: &[SortKey],
    mem_rows: usize,
) -> Result<Vec<Vec<Literal>>> {
    if rows.len() <= mem_rows {
        rows.sort_by(|a, b| cmp_rows(a, b, keys));
        return Ok(rows);
    }
    external_sort(rows, keys, mem_rows)
}

fn external_sort(
    rows: Vec<Vec<Literal>>,
    keys: &[SortKey],
    mem_rows: usize,
) -> Result<Vec<Vec<Literal>>> {
    let spill = SpillDir::new()?;
    let mut run_paths = Vec::new();

    // Phase 1: sorted runs of up to `mem_rows` each, spilled to disk.
    for (run_idx, chunk) in rows.chunks(mem_rows).enumerate() {
        let mut run: Vec<&Vec<Literal>> = chunk.iter().collect();
        run.sort_by(|a, b| cmp_rows(a, b, keys));
        let path = spill.path(run_idx);
        let mut w = BufWriter::new(File::create(&path)?);
        for row in run {
            write_row(&mut w, row)?;
        }
        w.flush()?;
        run_paths.push(path);
    }

    // Phase 2: streaming k-way merge, one row resident per run.
    let mut readers: Vec<BufReader<File>> = run_paths
        .iter()
        .map(|p| Ok(BufReader::new(File::open(p)?)))
        .collect::<Result<_>>()?;
    let mut heap: BinaryHeap<HeapItem> = BinaryHeap::new();
    for (i, reader) in readers.iter_mut().enumerate() {
        if let Some(row) = read_row(reader)? {
            heap.push(HeapItem { row, run: i, keys });
        }
    }

    let mut out = Vec::with_capacity(rows.len());
    while let Some(item) = heap.pop() {
        let run = item.run;
        out.push(item.row);
        if let Some(row) = read_row(&mut readers[run])? {
            heap.push(HeapItem { row, run, keys });
        }
    }
    Ok(out)
}

/// A row in the merge heap. `BinaryHeap` is a max-heap, so `Ord` is reversed
/// (and ties broken by run index) to make it pop the *smallest* row first.
struct HeapItem<'a> {
    row: Vec<Literal>,
    run: usize,
    keys: &'a [SortKey],
}

impl Ord for HeapItem<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        cmp_rows(&self.row, &other.row, self.keys)
            .then(self.run.cmp(&other.run))
            .reverse()
    }
}
impl PartialOrd for HeapItem<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for HeapItem<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for HeapItem<'_> {}

struct SpillDir {
    dir: PathBuf,
}

impl SpillDir {
    fn new() -> Result<Self> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("unidb-sort-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }
    fn path(&self, run: usize) -> PathBuf {
        self.dir.join(format!("run-{run}"))
    }
}
impl Drop for SpillDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_row(w: &mut BufWriter<File>, row: &[Literal]) -> Result<()> {
    let line = serde_json::to_string(row)
        .map_err(|e| DbError::SqlPlan(format!("sort spill encode: {e}")))?;
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    Ok(())
}

fn read_row(reader: &mut BufReader<File>) -> Result<Option<Vec<Literal>>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let row: Vec<Literal> = serde_json::from_str(trimmed)
        .map_err(|e| DbError::SqlPlan(format!("sort spill decode: {e}")))?;
    Ok(Some(row))
}
