//! P4.b hash aggregation over a materialized [`Batch`]. Groups input rows by
//! the group-key expressions and computes each [`AggCall`] per group. Output
//! rows are `[group keys..., aggregate results...]`, matching the synthetic
//! schema the planner built (`__g*` then `__a*`).
//!
//! Aggregate result typing follows SQLite so the differential tests line up:
//! `COUNT` -> integer (0 for an empty group), `SUM` -> integer when every
//! summed value is an integer else float (NULL if no non-null input), `AVG` ->
//! float (NULL if none), `MIN`/`MAX` -> the extreme value (NULL if none).

use std::collections::HashMap;

use crate::error::Result;
use crate::sql::executor::{self, encode_row};
use crate::sql::logical::Literal;
use crate::sql::plan::{eval_qexpr, AggCall, Batch, ColumnRef};
use crate::sql::query::{AggFunc, QExpr};

pub fn aggregate(
    input: Batch,
    group_exprs: &[QExpr],
    aggs: &[AggCall],
    output: &[ColumnRef],
) -> Result<Batch> {
    // Preserve first-seen group order for stable (if unspecified) output.
    let mut order: Vec<Vec<u8>> = Vec::new();
    let mut groups: HashMap<Vec<u8>, (Vec<Literal>, Vec<Acc>)> = HashMap::new();

    for row in &input.rows {
        let key: Vec<Literal> = group_exprs
            .iter()
            .map(|g| eval_qexpr(g, &input.schema, row))
            .collect::<Result<_>>()?;
        let key_bytes = encode_row(&key);
        let entry = groups.entry(key_bytes.clone()).or_insert_with(|| {
            order.push(key_bytes.clone());
            (key.clone(), aggs.iter().map(Acc::new).collect())
        });
        for (acc, call) in entry.1.iter_mut().zip(aggs) {
            match &call.arg {
                None => acc.update_star(),
                Some(arg) => {
                    let v = eval_qexpr(arg, &input.schema, row)?;
                    acc.update(v);
                }
            }
        }
    }

    let mut rows = Vec::new();
    if order.is_empty() && group_exprs.is_empty() {
        // No input rows and no GROUP BY: one row of "empty" aggregates
        // (COUNT -> 0, everything else -> NULL), matching SQL.
        let accs: Vec<Acc> = aggs.iter().map(Acc::new).collect();
        rows.push(accs.iter().map(Acc::finalize).collect());
    } else {
        for key_bytes in &order {
            let (key, accs) = &groups[key_bytes];
            let mut row = key.clone();
            row.extend(accs.iter().map(Acc::finalize));
            rows.push(row);
        }
    }

    Ok(Batch {
        schema: output.to_vec(),
        rows,
    })
}

/// Per-(group, aggregate) accumulator.
struct Acc {
    func: AggFunc,
    distinct: bool,
    seen: std::collections::HashSet<Vec<u8>>,
    count: i64,
    sum_i: i128,
    sum_f: f64,
    saw_float: bool,
    saw_any: bool,
    extreme: Option<Literal>,
}

impl Acc {
    fn new(call: &AggCall) -> Self {
        Acc {
            func: call.func,
            distinct: call.distinct,
            seen: std::collections::HashSet::new(),
            count: 0,
            sum_i: 0,
            sum_f: 0.0,
            saw_float: false,
            saw_any: false,
            extreme: None,
        }
    }

    /// `COUNT(*)` — counts every row regardless of NULLs.
    fn update_star(&mut self) {
        self.count += 1;
    }

    fn update(&mut self, v: Literal) {
        if matches!(v, Literal::Null) {
            return;
        }
        if self.distinct {
            let bytes = encode_row(std::slice::from_ref(&v));
            if !self.seen.insert(bytes) {
                return;
            }
        }
        self.count += 1;
        match self.func {
            AggFunc::Count => {}
            AggFunc::Sum => {
                self.saw_any = true;
                match &v {
                    Literal::Int(n) => {
                        self.sum_i += *n as i128;
                        self.sum_f += *n as f64;
                    }
                    other => {
                        self.saw_float = true;
                        self.sum_f += num_as_f64(other);
                    }
                }
            }
            AggFunc::Avg => {
                self.saw_any = true;
                self.sum_f += num_as_f64(&v);
            }
            AggFunc::Min | AggFunc::Max => {
                let replace = match &self.extreme {
                    None => true,
                    Some(cur) => match executor::literal_ord(&v, cur) {
                        Some(std::cmp::Ordering::Less) => matches!(self.func, AggFunc::Min),
                        Some(std::cmp::Ordering::Greater) => matches!(self.func, AggFunc::Max),
                        _ => false,
                    },
                };
                if replace {
                    self.extreme = Some(v);
                }
            }
        }
    }

    fn finalize(&self) -> Literal {
        match self.func {
            AggFunc::Count => Literal::Int(self.count),
            AggFunc::Sum => {
                if !self.saw_any {
                    Literal::Null
                } else if self.saw_float {
                    Literal::Float(self.sum_f)
                } else {
                    Literal::Int(self.sum_i as i64)
                }
            }
            AggFunc::Avg => {
                if self.count == 0 {
                    Literal::Null
                } else {
                    Literal::Float(self.sum_f / self.count as f64)
                }
            }
            AggFunc::Min | AggFunc::Max => self.extreme.clone().unwrap_or(Literal::Null),
        }
    }
}

/// Coerce a numeric literal to `f64` for SUM/AVG (Int/Float/Decimal). Non-numeric
/// values contribute 0.0 — the same permissive coercion SQLite applies to
/// summing text (it treats non-numeric as 0).
fn num_as_f64(l: &Literal) -> f64 {
    match l {
        Literal::Int(n) => *n as f64,
        Literal::Float(f) => *f,
        Literal::Decimal(v, s) => *v as f64 / 10f64.powi(*s as i32),
        _ => 0.0,
    }
}
