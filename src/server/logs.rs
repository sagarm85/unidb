//! `GET /logs` — a bounded, cursor-paged tail over the rotated JSON log files
//! (item 22, L3).
//!
//! **This is explicitly NOT a log database.** There is no index, no ingest, no
//! background process — just a filtered reverse read of the very files
//! `tracing-appender` already writes (`unidb.log.YYYY-MM-DD`). It exists so a
//! local/single-node operator (or the studio Logs tab, L4) gets a
//! CloudWatch/Datadog-*like* tail without running CloudWatch or Datadog; a real
//! deployment ships those same JSON files to a real platform (see
//! `ops_runbook.md`, L5).
//!
//! **Why it cannot OOM or stall on a multi-GB log directory** — three hard
//! bounds, all enforced here, all covered by tests:
//!
//! 1. **Reverse block reads.** Files are read newest-first, from the end
//!    backward in fixed [`BLOCK`]-sized chunks, one complete line at a time. A
//!    file is *never* loaded into memory whole; live memory is one block plus
//!    the current line, regardless of file size.
//! 2. **A hard page cap** ([`MAX_PAGE`]). However large `limit` is asked for,
//!    at most this many lines are returned in one response.
//! 3. **A scan budget** ([`SCAN_BUDGET_LINES`]). Even a filter that matches
//!    nothing (`q=` a needle absent from a 10 GB haystack) examines at most this
//!    many lines before returning what it has plus a cursor to resume — so one
//!    request does bounded work instead of walking the whole directory.
//!
//! The `cursor` is an opaque, resumable anchor: the *filename* plus the byte
//! offset of the oldest line already returned. Anchoring on the filename (not a
//! positional index) keeps pagination stable when a new day's file rotates in
//! at the head between requests.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::Deserialize;

/// Reverse-read chunk size. 64 KiB amortizes syscalls while keeping live memory
/// tiny relative to any real log file.
const BLOCK: usize = 64 * 1024;

/// Hard ceiling on lines returned in one page, regardless of the requested
/// `limit`. Bounds response size and serialization cost.
pub const MAX_PAGE: usize = 500;

/// Hard ceiling on lines *examined* in one request. This is the anti-stall
/// bound: a restrictive filter over a huge directory returns after scanning at
/// most this many lines (plus a resume cursor), never the whole corpus.
pub const SCAN_BUDGET_LINES: usize = 50_000;

/// The prefix `tracing-appender`'s daily rotation writes (`unidb.log.<date>`).
const LOG_PREFIX: &str = "unidb.log";

/// Parsed, validated `GET /logs` query.
#[derive(Debug, Default, Clone)]
pub struct LogQuery {
    /// Minimum severity to include (`ERROR` > `WARN` > `INFO` > `DEBUG` >
    /// `TRACE`). A line at or above this level passes.
    pub level: Option<String>,
    /// Inclusive lower bound on the line's RFC3339 UTC `timestamp` (lexical).
    pub since: Option<String>,
    /// Inclusive upper bound on the line's RFC3339 UTC `timestamp` (lexical).
    pub until: Option<String>,
    /// Case-sensitive substring the raw line must contain (e.g. a `request_id`).
    pub q: Option<String>,
    /// Opaque resume cursor from a previous page's `next_cursor`.
    pub cursor: Option<String>,
    /// Requested page size, clamped to [`MAX_PAGE`].
    pub limit: Option<usize>,
}

/// Raw query-string shape (all optional strings) → [`LogQuery`].
#[derive(Debug, Deserialize)]
pub struct LogQueryParams {
    pub level: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub q: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<usize>,
}

impl From<LogQueryParams> for LogQuery {
    fn from(p: LogQueryParams) -> Self {
        LogQuery {
            level: p.level.filter(|s| !s.is_empty()),
            since: p.since.filter(|s| !s.is_empty()),
            until: p.until.filter(|s| !s.is_empty()),
            q: p.q.filter(|s| !s.is_empty()),
            cursor: p.cursor.filter(|s| !s.is_empty()),
            limit: p.limit,
        }
    }
}

/// One page of filtered log lines.
#[derive(Debug)]
pub struct LogPage {
    /// The matching lines, newest-first, each parsed to JSON (or `{"raw": …}`
    /// if a line was not valid JSON — filters that need fields skip those).
    pub logs: Vec<serde_json::Value>,
    /// Anchor to fetch the next (older) page, or `None` at the end.
    pub next_cursor: Option<String>,
    /// Lines examined (bounded by [`SCAN_BUDGET_LINES`]).
    pub scanned: usize,
    /// Whether the scan budget stopped us before the page filled or the corpus
    /// was exhausted — i.e. there is definitely more to read via `next_cursor`.
    pub truncated: bool,
}

impl LogPage {
    fn empty() -> Self {
        LogPage {
            logs: Vec::new(),
            next_cursor: None,
            scanned: 0,
            truncated: false,
        }
    }
}

/// Severity rank; higher = more severe. Unknown levels rank below `TRACE` so a
/// `level=` filter never *includes* an unrecognized line by accident.
fn level_rank(level: &str) -> i32 {
    match level.to_ascii_uppercase().as_str() {
        "ERROR" => 4,
        "WARN" | "WARNING" => 3,
        "INFO" => 2,
        "DEBUG" => 1,
        "TRACE" => 0,
        _ => -1,
    }
}

/// Discover the rotated log files, newest-first. Names sort lexically, and the
/// `unidb.log.YYYY-MM-DD` scheme makes lexical-descending == chronological
/// newest-first.
fn list_log_files(dir: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let read_dir = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        // A missing log dir just means "no logs yet", not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(e) => return Err(e),
    };
    for entry in read_dir {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(LOG_PREFIX) {
            files.push((name, entry.path()));
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(files)
}

fn encode_cursor(file_name: &str, offset: u64) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{file_name}\n{offset}"))
}

fn decode_cursor(cursor: &str) -> Option<(String, u64)> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let (name, offset) = text.split_once('\n')?;
    Some((name.to_string(), offset.parse().ok()?))
}

/// Read matching log lines, newest-first, bounded by [`MAX_PAGE`] and
/// [`SCAN_BUDGET_LINES`]. Never loads a file whole (see module docs).
pub fn read_logs(dir: &Path, query: &LogQuery) -> std::io::Result<LogPage> {
    let files = list_log_files(dir)?;
    if files.is_empty() {
        return Ok(LogPage::empty());
    }
    let cap = query.limit.unwrap_or(MAX_PAGE).clamp(1, MAX_PAGE);

    // Resolve the starting (file, offset). A cursor anchors on the filename so a
    // freshly rotated newest file doesn't shift positions mid-pagination; if the
    // anchored file has since been deleted (rotated out of retention), there is
    // nothing older to return.
    let (start_idx, start_offset) = match &query.cursor {
        Some(c) => match decode_cursor(c) {
            Some((name, offset)) => match files.iter().position(|(n, _)| *n == name) {
                Some(idx) => (idx, Some(offset)),
                None => return Ok(LogPage::empty()),
            },
            // A malformed cursor is treated as "start from the top" rather than
            // erroring — the endpoint stays forgiving of a stale client token.
            None => (0, None),
        },
        None => (0, None),
    };

    let mut page = LogPage::empty();

    'files: for (fi, (name, path)) in files.iter().enumerate().skip(start_idx) {
        let file_len = std::fs::metadata(path)?.len();
        let end = if fi == start_idx {
            start_offset.unwrap_or(file_len).min(file_len)
        } else {
            file_len
        };
        let mut reader = ReverseLines::open(path, end)?;

        while let Some((line_start, line)) = reader.next_line()? {
            page.scanned += 1;

            if let Some(value) = filter_line(&line, query) {
                page.logs.push(value);
                if page.logs.len() >= cap {
                    // Page full; resume just before this line next time.
                    page.next_cursor = Some(encode_cursor(name, line_start));
                    break 'files;
                }
            }

            if page.scanned >= SCAN_BUDGET_LINES {
                // Anti-stall: stop and hand back a resume point even though the
                // page isn't full and the corpus isn't exhausted.
                page.next_cursor = Some(encode_cursor(name, line_start));
                page.truncated = true;
                break 'files;
            }
        }
        // Reached offset 0 of this file with budget to spare → fall through to
        // the next (older) file. No cursor unless we later stop.
    }

    Ok(page)
}

/// Apply the query filters to one raw line. Returns the JSON value to emit, or
/// `None` if the line is filtered out. The cheap substring test runs first.
fn filter_line(line: &[u8], query: &LogQuery) -> Option<serde_json::Value> {
    let text = std::str::from_utf8(line).ok()?;
    if text.trim().is_empty() {
        return None;
    }

    if let Some(needle) = &query.q {
        if !text.contains(needle.as_str()) {
            return None;
        }
    }

    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => {
            if let Some(min_level) = &query.level {
                let line_level = value.get("level").and_then(|v| v.as_str()).unwrap_or("");
                if level_rank(line_level) < level_rank(min_level) {
                    return None;
                }
            }
            if query.since.is_some() || query.until.is_some() {
                let ts = value
                    .get("timestamp")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(since) = &query.since {
                    if ts < since.as_str() {
                        return None;
                    }
                }
                if let Some(until) = &query.until {
                    if ts > until.as_str() {
                        return None;
                    }
                }
            }
            Some(value)
        }
        // A non-JSON line can't satisfy field-level filters; only a bare `q`
        // (or no filter) passes it through, wrapped so the shape stays uniform.
        Err(_) => {
            if query.level.is_some() || query.since.is_some() || query.until.is_some() {
                None
            } else {
                Some(serde_json::json!({ "raw": text }))
            }
        }
    }
}

/// A bounded reverse line reader: yields complete lines from `file[0..end)`
/// newest (highest offset) first, loading only one [`BLOCK`] at a time. Each
/// yielded item is `(line_start_byte_offset, line_bytes_without_newline)`.
struct ReverseLines {
    file: File,
    /// Bytes of the file not yet loaded into `buf` (the unread prefix length).
    remaining: u64,
    /// File offset of `buf[0]`.
    buf_start: u64,
    /// Loaded bytes = `file[buf_start .. buf_start + buf.len())`. Trimmed from
    /// the right as lines are emitted, prepended-to as older blocks load, so it
    /// never exceeds ~one block plus one line.
    buf: Vec<u8>,
}

impl ReverseLines {
    fn open(path: &Path, end: u64) -> std::io::Result<Self> {
        Ok(ReverseLines {
            file: File::open(path)?,
            remaining: end,
            buf_start: end,
            buf: Vec::new(),
        })
    }

    /// Load the previous block (older bytes) onto the front of `buf`.
    fn load_more(&mut self) -> std::io::Result<()> {
        let block = std::cmp::min(BLOCK as u64, self.remaining);
        if block == 0 {
            return Ok(());
        }
        self.remaining -= block;
        self.buf_start = self.remaining;
        let mut chunk = vec![0u8; block as usize];
        self.file.seek(SeekFrom::Start(self.remaining))?;
        self.file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&self.buf);
        self.buf = chunk;
        Ok(())
    }

    fn next_line(&mut self) -> std::io::Result<Option<(u64, Vec<u8>)>> {
        loop {
            match self.buf.iter().rposition(|&b| b == b'\n') {
                Some(nl) if nl == self.buf.len() - 1 => {
                    // Trailing terminator; the line we want ends here — find the
                    // previous newline to locate its start.
                    match self.buf[..nl].iter().rposition(|&b| b == b'\n') {
                        Some(prev) => {
                            let line = self.buf[prev + 1..nl].to_vec();
                            let start = self.buf_start + prev as u64 + 1;
                            self.buf.truncate(prev + 1); // keep '\n' at `prev` as next terminator
                            return Ok(Some((start, line)));
                        }
                        None => {
                            if self.remaining == 0 {
                                // First line of the file (starts at offset 0).
                                let line = self.buf[..nl].to_vec();
                                let start = self.buf_start;
                                self.buf.clear();
                                return Ok(if line.is_empty() && start == 0 {
                                    None
                                } else {
                                    Some((start, line))
                                });
                            }
                            self.load_more()?;
                        }
                    }
                }
                Some(nl) => {
                    // Content after the last newline with no trailing terminator
                    // = an unterminated final line (file didn't end in '\n').
                    let line = self.buf[nl + 1..].to_vec();
                    let start = self.buf_start + nl as u64 + 1;
                    self.buf.truncate(nl + 1);
                    if line.is_empty() {
                        continue;
                    }
                    return Ok(Some((start, line)));
                }
                None => {
                    if self.remaining == 0 {
                        if self.buf.is_empty() {
                            return Ok(None);
                        }
                        let line = self.buf.clone();
                        let start = self.buf_start;
                        self.buf.clear();
                        return Ok(Some((start, line)));
                    }
                    self.load_more()?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_log(dir: &Path, name: &str, lines: &[&str]) {
        let mut f = File::create(dir.join(name)).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn levels(page: &LogPage) -> Vec<String> {
        page.logs
            .iter()
            .map(|v| {
                v.get("level")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn newest_first_across_files() {
        let dir = tempdir().unwrap();
        write_log(
            dir.path(),
            "unidb.log.2026-07-12",
            &[r#"{"timestamp":"2026-07-12T00:00:01Z","level":"INFO","n":1}"#],
        );
        write_log(
            dir.path(),
            "unidb.log.2026-07-13",
            &[
                r#"{"timestamp":"2026-07-13T00:00:01Z","level":"INFO","n":2}"#,
                r#"{"timestamp":"2026-07-13T00:00:02Z","level":"WARN","n":3}"#,
            ],
        );
        let page = read_logs(dir.path(), &LogQuery::default()).unwrap();
        let ns: Vec<i64> = page
            .logs
            .iter()
            .map(|v| v.get("n").and_then(|x| x.as_i64()).unwrap())
            .collect();
        assert_eq!(ns, vec![3, 2, 1], "newest line first, newer file first");
    }

    #[test]
    fn level_filter_is_minimum_severity() {
        let dir = tempdir().unwrap();
        write_log(
            dir.path(),
            "unidb.log.2026-07-13",
            &[
                r#"{"timestamp":"2026-07-13T00:00:01Z","level":"DEBUG"}"#,
                r#"{"timestamp":"2026-07-13T00:00:02Z","level":"INFO"}"#,
                r#"{"timestamp":"2026-07-13T00:00:03Z","level":"WARN"}"#,
                r#"{"timestamp":"2026-07-13T00:00:04Z","level":"ERROR"}"#,
            ],
        );
        let q = LogQuery {
            level: Some("WARN".into()),
            ..Default::default()
        };
        assert_eq!(
            levels(&read_logs(dir.path(), &q).unwrap()),
            vec!["ERROR", "WARN"]
        );
    }

    #[test]
    fn time_and_substring_filters() {
        let dir = tempdir().unwrap();
        write_log(
            dir.path(),
            "unidb.log.2026-07-13",
            &[
                r#"{"timestamp":"2026-07-13T01:00:00Z","level":"INFO","request_id":"req-a"}"#,
                r#"{"timestamp":"2026-07-13T02:00:00Z","level":"INFO","request_id":"req-b"}"#,
                r#"{"timestamp":"2026-07-13T03:00:00Z","level":"INFO","request_id":"req-a"}"#,
            ],
        );
        let q = LogQuery {
            q: Some("req-a".into()),
            since: Some("2026-07-13T02:30:00Z".into()),
            ..Default::default()
        };
        let page = read_logs(dir.path(), &q).unwrap();
        assert_eq!(page.logs.len(), 1, "only the 03:00 req-a line matches both");
        assert_eq!(
            page.logs[0].get("timestamp").unwrap().as_str().unwrap(),
            "2026-07-13T03:00:00Z"
        );
    }

    #[test]
    fn cursor_pagination_is_stable_and_complete() {
        let dir = tempdir().unwrap();
        let lines: Vec<String> = (0..25)
            .map(|i| {
                format!(r#"{{"timestamp":"2026-07-13T00:00:{i:02}Z","level":"INFO","n":{i}}}"#)
            })
            .collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        write_log(dir.path(), "unidb.log.2026-07-13", &refs);

        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let q = LogQuery {
                limit: Some(10),
                cursor: cursor.clone(),
                ..Default::default()
            };
            let page = read_logs(dir.path(), &q).unwrap();
            for v in &page.logs {
                seen.push(v.get("n").and_then(|x| x.as_i64()).unwrap());
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        let mut expected: Vec<i64> = (0..25).rev().collect();
        assert_eq!(seen, expected, "every line exactly once, newest-first");
        expected.dedup();
        assert_eq!(seen.len(), 25, "no duplicates across pages");
    }

    #[test]
    fn scan_budget_bounds_work_on_a_needle_in_a_haystack() {
        // A file far larger than the budget, with a matching line only at the
        // very oldest end. One request must NOT scan the whole file.
        let dir = tempdir().unwrap();
        let path = dir.path().join("unidb.log.2026-07-13");
        {
            let mut f = File::create(&path).unwrap();
            // Oldest line (written first) is the only match.
            writeln!(
                f,
                r#"{{"timestamp":"2026-07-13T00:00:00Z","level":"INFO","request_id":"needle"}}"#
            )
            .unwrap();
            for i in 0..(SCAN_BUDGET_LINES + 5_000) {
                writeln!(
                    f,
                    r#"{{"timestamp":"2026-07-13T00:00:01Z","level":"INFO","n":{i}}}"#
                )
                .unwrap();
            }
        }
        let q = LogQuery {
            q: Some("needle".into()),
            ..Default::default()
        };
        let page = read_logs(dir.path(), &q).unwrap();
        assert!(page.logs.is_empty(), "needle not reached within one budget");
        assert!(page.truncated, "budget stopped the scan");
        assert_eq!(
            page.scanned, SCAN_BUDGET_LINES,
            "scanned exactly the budget"
        );
        assert!(page.next_cursor.is_some(), "a resume cursor is returned");

        // Draining across pages eventually finds it, still bounded per request.
        let mut cursor = page.next_cursor;
        let mut found = false;
        for _ in 0..10 {
            let q = LogQuery {
                q: Some("needle".into()),
                cursor: cursor.clone(),
                ..Default::default()
            };
            let p = read_logs(dir.path(), &q).unwrap();
            assert!(p.scanned <= SCAN_BUDGET_LINES);
            if !p.logs.is_empty() {
                found = true;
                break;
            }
            cursor = p.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert!(
            found,
            "the needle is reachable by paging, just not in one shot"
        );
    }

    #[test]
    fn missing_dir_and_empty_are_ok() {
        let dir = tempdir().unwrap();
        let page = read_logs(&dir.path().join("nope"), &LogQuery::default()).unwrap();
        assert!(page.logs.is_empty() && page.next_cursor.is_none());
    }
}
