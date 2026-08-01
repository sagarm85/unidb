//! Minimal, dependency-free standard 5-field cron expression parser +
//! matcher (item 144: scheduled jobs). **Not** feature-gated — this backs
//! `Engine::upsert_cron_job`'s registration-time validation, which must work
//! for the embedded (no-`server`-feature) API exactly like the HTTP admin
//! route, so it lives at the crate root next to `authz`, not under
//! `server/`.
//!
//! Deliberately hand-rolled instead of pulling in a `cron`/`saffron`
//! dependency (CLAUDE.md's "no heavy dep" + this crate's general aversion to
//! extra supply-chain surface for a feature this small): a 5-field cron
//! grammar (`*`, a bare number, a range `a-b`, a step `*/n` or `a-b/n`, and
//! comma-separated lists of any of those) is a small, easily-tested state
//! machine.
//!
//! This module is pure calendar-*field* matching — it has no notion of "now"
//! or timezones at all. The (`server`-feature-gated) scheduler worker in
//! `server::cron` is the one piece that turns a real timestamp into local
//! calendar fields (minute/hour/day-of-month/month/day-of-week) via `chrono`
//! and feeds them into [`CronSchedule::matches`]; keeping that timestamp
//! math out of this module is what lets registration-time validation stay
//! dependency-free and usable with no `server` feature enabled.

use std::collections::BTreeSet;

/// One of the 5 fields of a standard cron expression, already validated
/// against its own `[min, max]` range.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CronField {
    /// `true` only when the field's source text was exactly `*` — used for
    /// the day-of-month/day-of-week OR-vs-AND cron semantics in
    /// [`CronSchedule::matches`]. A field that happens to enumerate every
    /// value via an explicit list (e.g. `0-59`) is intentionally NOT treated
    /// as wildcard here — only literal `*` gets that special day-field
    /// meaning, matching every real cron implementation.
    wildcard: bool,
    values: BTreeSet<u32>,
}

impl CronField {
    fn parse(raw: &str, min: u32, max: u32) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("field must not be empty".to_string());
        }
        if raw == "*" {
            return Ok(Self {
                wildcard: true,
                values: (min..=max).collect(),
            });
        }
        let mut values = BTreeSet::new();
        for part in raw.split(',') {
            if part.is_empty() {
                return Err(format!("empty field component in '{raw}'"));
            }
            let (range_part, step) = match part.split_once('/') {
                Some((r, s)) => {
                    let step: u32 = s
                        .parse()
                        .map_err(|_| format!("invalid step '{s}' in '{part}'"))?;
                    if step == 0 {
                        return Err(format!("step must be greater than 0 in '{part}'"));
                    }
                    (r, step)
                }
                None => (part, 1),
            };
            let (lo, hi) = if range_part == "*" {
                (min, max)
            } else if let Some((a, b)) = range_part.split_once('-') {
                let lo: u32 = a
                    .parse()
                    .map_err(|_| format!("invalid range start '{a}' in '{part}'"))?;
                let hi: u32 = b
                    .parse()
                    .map_err(|_| format!("invalid range end '{b}' in '{part}'"))?;
                if lo > hi {
                    return Err(format!(
                        "range start {lo} is greater than end {hi} in '{part}'"
                    ));
                }
                (lo, hi)
            } else {
                let v: u32 = range_part
                    .parse()
                    .map_err(|_| format!("invalid value '{range_part}' in '{part}'"))?;
                (v, v)
            };
            if lo < min || hi > max {
                return Err(format!(
                    "value out of range [{min},{max}] in '{part}' (field '{raw}')"
                ));
            }
            let mut v = lo;
            while v <= hi {
                values.insert(v);
                v += step;
            }
        }
        if values.is_empty() {
            // Unreachable in practice (every parse path above inserts at
            // least one value on success) — defensive, not a dead branch a
            // future edit could silently reintroduce.
            return Err(format!("field '{raw}' matches no values"));
        }
        Ok(Self {
            wildcard: false,
            values,
        })
    }

    fn matches(&self, value: u32) -> bool {
        self.values.contains(&value)
    }
}

/// A parsed, validated standard 5-field cron expression: `minute hour
/// day-of-month month day-of-week`. Day-of-week accepts `0`-`7` (both `0`
/// and `7` mean Sunday, the common cron convention); every other field is a
/// plain numeric range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CronSchedule {
    minute: CronField,
    hour: CronField,
    dom: CronField,
    month: CronField,
    dow: CronField,
}

impl CronSchedule {
    /// Parse and validate a standard 5-field cron expression. Returns a
    /// human-readable error — surfaced as `400 INVALID_CRON_SCHEDULE` by the
    /// registration route (`Engine::upsert_cron_job` calls this before
    /// storing anything) — for anything malformed: wrong field count,
    /// out-of-range values, non-numeric components, a zero step, an
    /// inverted range, etc.
    pub fn parse(expr: &str) -> Result<Self, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron expression must have exactly 5 whitespace-separated fields \
                 (minute hour day-of-month month day-of-week), got {} in '{expr}'",
                fields.len()
            ));
        }
        let minute = CronField::parse(fields[0], 0, 59)?;
        let hour = CronField::parse(fields[1], 0, 23)?;
        let dom = CronField::parse(fields[2], 1, 31)?;
        let month = CronField::parse(fields[3], 1, 12)?;
        let mut dow = CronField::parse(fields[4], 0, 7)?;
        // Normalize the "7 also means Sunday" alias onto 0 so `matches`
        // only ever has to compare against a 0-6 day-of-week value.
        if dow.values.remove(&7) {
            dow.values.insert(0);
        }
        Ok(Self {
            minute,
            hour,
            dom,
            month,
            dow,
        })
    }

    /// Whether this schedule fires at the given local calendar fields.
    /// `dow` is `0` (Sunday) through `6` (Saturday).
    ///
    /// Standard cron day semantics: if BOTH day-of-month and day-of-week are
    /// restricted (source text was not literal `*`), the day matches when
    /// EITHER one matches (OR, not AND — the one real surprise in the cron
    /// grammar); if only one of the two is restricted, that one alone
    /// governs (the wildcard field can never itself veto a match).
    pub fn matches(&self, minute: u32, hour: u32, dom: u32, month: u32, dow: u32) -> bool {
        if !self.minute.matches(minute) || !self.hour.matches(hour) || !self.month.matches(month) {
            return false;
        }
        match (self.dom.wildcard, self.dow.wildcard) {
            (true, true) => true,
            (true, false) => self.dow.matches(dow),
            (false, true) => self.dom.matches(dom),
            (false, false) => self.dom.matches(dom) || self.dow.matches(dow),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_minute_matches_everything() {
        let s = CronSchedule::parse("* * * * *").unwrap();
        assert!(s.matches(0, 0, 1, 1, 0));
        assert!(s.matches(59, 23, 31, 12, 6));
        assert!(s.matches(30, 12, 15, 6, 3));
    }

    #[test]
    fn exact_fields_match_only_that_instant() {
        // 09:30 on the 1st of every month, any day-of-week.
        let s = CronSchedule::parse("30 9 1 * *").unwrap();
        assert!(s.matches(30, 9, 1, 3, 2));
        assert!(!s.matches(31, 9, 1, 3, 2)); // wrong minute
        assert!(!s.matches(30, 10, 1, 3, 2)); // wrong hour
        assert!(!s.matches(30, 9, 2, 3, 2)); // wrong day-of-month
    }

    #[test]
    fn comma_list_matches_any_listed_value() {
        let s = CronSchedule::parse("0,15,30,45 * * * *").unwrap();
        for m in [0u32, 15, 30, 45] {
            assert!(s.matches(m, 0, 1, 1, 0));
        }
        assert!(!s.matches(20, 0, 1, 1, 0));
    }

    #[test]
    fn range_matches_inclusive_bounds() {
        let s = CronSchedule::parse("0 9-17 * * *").unwrap();
        assert!(s.matches(0, 9, 1, 1, 0));
        assert!(s.matches(0, 17, 1, 1, 0));
        assert!(!s.matches(0, 8, 1, 1, 0));
        assert!(!s.matches(0, 18, 1, 1, 0));
    }

    #[test]
    fn step_matches_every_nth_value_from_start() {
        let s = CronSchedule::parse("*/15 * * * *").unwrap();
        for m in [0u32, 15, 30, 45] {
            assert!(s.matches(m, 0, 1, 1, 0));
        }
        for m in [1u32, 14, 16, 44] {
            assert!(!s.matches(m, 0, 1, 1, 0));
        }
    }

    #[test]
    fn range_with_step() {
        let s = CronSchedule::parse("0 8-20/4 * * *").unwrap();
        for h in [8u32, 12, 16, 20] {
            assert!(s.matches(0, h, 1, 1, 0));
        }
        assert!(!s.matches(0, 10, 1, 1, 0));
    }

    #[test]
    fn dom_and_dow_are_ored_when_both_restricted() {
        // 15th of the month OR every Monday, per standard cron semantics.
        let s = CronSchedule::parse("0 0 15 * 1").unwrap();
        assert!(s.matches(0, 0, 15, 6, 3)); // matches on day-of-month alone
        assert!(s.matches(0, 0, 3, 6, 1)); // matches on day-of-week alone
        assert!(!s.matches(0, 0, 3, 6, 2)); // matches neither
    }

    #[test]
    fn dow_seven_aliases_to_sunday() {
        let s = CronSchedule::parse("0 0 * * 7").unwrap();
        assert!(s.matches(0, 0, 1, 1, 0)); // dow=0 (Sunday)
        assert!(!s.matches(0, 0, 1, 1, 7)); // never passed in but be defensive
    }

    #[test]
    fn rejects_wrong_field_count() {
        assert!(CronSchedule::parse("* * * *").is_err());
        assert!(CronSchedule::parse("* * * * * *").is_err());
        assert!(CronSchedule::parse("").is_err());
    }

    #[test]
    fn rejects_out_of_range_values() {
        assert!(CronSchedule::parse("60 * * * *").is_err()); // minute max 59
        assert!(CronSchedule::parse("* 24 * * *").is_err()); // hour max 23
        assert!(CronSchedule::parse("* * 0 * *").is_err()); // dom min 1
        assert!(CronSchedule::parse("* * 32 * *").is_err()); // dom max 31
        assert!(CronSchedule::parse("* * * 13 *").is_err()); // month max 12
        assert!(CronSchedule::parse("* * * * 8").is_err()); // dow max 7
    }

    #[test]
    fn rejects_malformed_syntax() {
        assert!(CronSchedule::parse("* * * * abc").is_err());
        assert!(CronSchedule::parse("* * * * 1-").is_err());
        assert!(CronSchedule::parse("* * * * 5-1").is_err()); // inverted range
        assert!(CronSchedule::parse("*/0 * * * *").is_err()); // zero step
        assert!(CronSchedule::parse("*,, * * * *").is_err()); // empty component
    }
}
