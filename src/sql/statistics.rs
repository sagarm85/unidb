//! P4.d table statistics: what `ANALYZE` collects and how the cost-based
//! optimizer ([`crate::sql::optimizer`]) estimates selectivity from it.
//!
//! Stats are stored on [`crate::catalog::TableDef`] and persisted through the
//! catalog's existing WAL-logged page write, so they are **durable and never
//! recomputed on open** — the same "no rebuild on open" property Phase 3
//! established for indexes.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::catalog::ColumnDef;
use crate::sql::executor::{encode_row, literal_ord};
use crate::sql::logical::{CmpOp, Literal};

/// Per-table statistics gathered by `ANALYZE`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TableStats {
    pub row_count: u64,
    pub columns: HashMap<String, ColumnStats>,
}

/// Per-column statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnStats {
    /// Number of distinct non-NULL values.
    pub distinct: u64,
    /// Number of NULL values.
    pub null_count: u64,
    pub min: Option<Literal>,
    pub max: Option<Literal>,
    /// Equi-depth histogram bucket upper bounds (sorted). Each bucket holds
    /// roughly the same number of rows; `bounds[i]` is the largest value in
    /// bucket `i`. Empty when the column has no orderable non-NULL values.
    pub bounds: Vec<Literal>,
}

// Kept small so per-column stats stay compact: the whole catalog (all
// TableDefs + all stats) is persisted as a single ~8 KiB page blob, so a wide
// schema's histograms must not balloon it. 8 equi-depth buckets is ample for
// range-selectivity estimation. (A multi-page catalog is tracked tech debt.)
const HISTOGRAM_BUCKETS: usize = 8;

/// Compute statistics for a table from its live rows (one `Vec<Literal>` per
/// row, in declaration order including dropped columns as NULL slots).
pub fn compute(rows: &[Vec<Literal>], columns: &[ColumnDef]) -> TableStats {
    let mut stats = TableStats {
        row_count: rows.len() as u64,
        columns: HashMap::new(),
    };
    for (idx, col) in columns.iter().enumerate() {
        if col.dropped {
            continue;
        }
        stats
            .columns
            .insert(col.name.clone(), column_stats(rows, idx));
    }
    stats
}

fn column_stats(rows: &[Vec<Literal>], idx: usize) -> ColumnStats {
    let mut distinct_keys = std::collections::HashSet::new();
    let mut null_count = 0u64;
    let mut values: Vec<Literal> = Vec::new();
    for row in rows {
        let v = &row[idx];
        if matches!(v, Literal::Null) {
            null_count += 1;
            continue;
        }
        distinct_keys.insert(encode_row(std::slice::from_ref(v)));
        values.push(v.clone());
    }
    // Sort orderable values for min/max + histogram; unorderable pairs are
    // treated as equal (leaves them wherever they land — harmless for stats).
    values.sort_by(|a, b| literal_ord(a, b).unwrap_or(std::cmp::Ordering::Equal));
    let min = values.first().cloned();
    let max = values.last().cloned();

    // Equi-depth bounds: pick the value at each bucket boundary.
    let mut bounds = Vec::new();
    if !values.is_empty() {
        let buckets = HISTOGRAM_BUCKETS.min(values.len());
        for b in 1..=buckets {
            let pos = (b * values.len() / buckets).saturating_sub(1);
            bounds.push(values[pos].clone());
        }
    }

    ColumnStats {
        distinct: distinct_keys.len() as u64,
        null_count,
        min,
        max,
        bounds,
    }
}

impl ColumnStats {
    /// Estimated fraction of rows (0.0..=1.0) matching `col <op> value`, from
    /// the uniform-distribution assumption for equality (1/distinct) and the
    /// equi-depth histogram / min-max for ranges. Returns `None` for operators
    /// or types the estimator can't reason about (the caller falls back to a
    /// default).
    pub fn selectivity(&self, op: CmpOp, value: &Literal, total: u64) -> Option<f64> {
        if total == 0 {
            return Some(0.0);
        }
        let non_null = total.saturating_sub(self.null_count) as f64;
        let frac_non_null = non_null / total as f64;
        match op {
            CmpOp::Eq => {
                if self.distinct == 0 {
                    return Some(0.0);
                }
                // Outside the observed range -> no match.
                if let (Some(min), Some(max)) = (&self.min, &self.max) {
                    if literal_ord(value, min).is_some_and(|o| o.is_lt())
                        || literal_ord(value, max).is_some_and(|o| o.is_gt())
                    {
                        return Some(0.0);
                    }
                }
                Some(frac_non_null / self.distinct as f64)
            }
            CmpOp::Ne => self
                .selectivity(CmpOp::Eq, value, total)
                .map(|eq| (1.0 - eq).max(0.0)),
            CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => {
                let below = self.fraction_below(value)?;
                let sel = match op {
                    CmpOp::Lt | CmpOp::Le => below,
                    CmpOp::Gt | CmpOp::Ge => 1.0 - below,
                    _ => unreachable!(),
                };
                Some((sel * frac_non_null).clamp(0.0, 1.0))
            }
        }
    }

    /// Fraction of non-NULL values `<= value`, estimated from the equi-depth
    /// bucket bounds. `None` if the column has no orderable bounds.
    fn fraction_below(&self, value: &Literal) -> Option<f64> {
        if self.bounds.is_empty() {
            return None;
        }
        // Count bounds strictly below `value`; each bound represents ~1/n of
        // the rows. This is a coarse but monotonic estimate.
        let n = self.bounds.len() as f64;
        let mut below = 0.0;
        for b in &self.bounds {
            match literal_ord(b, value)? {
                std::cmp::Ordering::Less | std::cmp::Ordering::Equal => below += 1.0,
                std::cmp::Ordering::Greater => break,
            }
        }
        Some((below / n).clamp(0.0, 1.0))
    }
}
