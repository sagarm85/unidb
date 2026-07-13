// Audit log (P6.f).
//
// A security-relevant, append-only trail of *who did what*: authorization DDL
// (CREATE/DROP USER|ROLE, GRANT, REVOKE), and every access decision for a named
// (non-embedded) user — both allowed privileged operations and denials. Written
// as one JSON object per line to `audit.log` in the data directory, so it is
// greppable and shippable to a SIEM without a parser.
//
// The implicit embedded superuser (identity `None`) is **not** audited: it is
// the trusted operator of the process. Auditing kicks in for named users and
// for auth DDL specifically, which is the trail a security review wants.
//
// Cheap and synchronous (an fsync-free append under a mutex); the audit path is
// only hit on control-plane / named-user statements, not the raw CRUD hot path.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::error::Result;

#[derive(Serialize)]
struct AuditEvent<'a> {
    /// Unix epoch micros.
    ts: u128,
    /// The acting user (`"<embedded>"` for the implicit superuser, though those
    /// are not normally logged).
    user: &'a str,
    /// A short action verb (`"grant"`, `"select"`, `"create_user"`, …).
    action: &'a str,
    /// The object acted on (a table, user, or role name; empty when N/A).
    object: &'a str,
    /// Whether the action was permitted.
    allowed: bool,
    /// The transaction id the statement ran under (item 22, L2). `None` for
    /// auth DDL evaluated outside a data transaction. Correlates an audit line
    /// with the app log's `txn_id` span field.
    #[serde(skip_serializing_if = "Option::is_none")]
    txn_id: Option<u64>,
    /// The originating HTTP `request_id` (item 22, L2), read from the
    /// per-thread correlation context the server sets on the blocking call.
    /// Absent for the embedded API (no request). Lets a security review pull a
    /// request's app-log, slow-query, and audit lines by one id.
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
}

/// Append-only audit trail. `Send + Sync` for the shared `Engine`.
pub struct AuditLog {
    file: Mutex<File>,
}

impl AuditLog {
    /// Open (create/append) `audit.log` in `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("audit.log"))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Record one access decision. `user == None` (the embedded superuser) is a
    /// no-op — only named users and auth DDL are audited. `txn_id` is the
    /// transaction the statement ran under (item 22, L2).
    pub fn record(
        &self,
        user: Option<&str>,
        txn_id: Option<u64>,
        action: &str,
        object: &str,
        allowed: bool,
    ) {
        let Some(user) = user else { return };
        self.write_event(user, txn_id, action, object, allowed);
    }

    /// Record an auth-DDL event (always logged, even for the embedded superuser,
    /// since changing the authorization graph is itself security-relevant).
    pub fn record_admin(
        &self,
        user: Option<&str>,
        txn_id: Option<u64>,
        action: &str,
        object: &str,
        allowed: bool,
    ) {
        self.write_event(
            user.unwrap_or("<embedded>"),
            txn_id,
            action,
            object,
            allowed,
        );
    }

    fn write_event(
        &self,
        user: &str,
        txn_id: Option<u64>,
        action: &str,
        object: &str,
        allowed: bool,
    ) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0);
        // Correlation id from the server's per-thread context (item 22, L2);
        // `None` for the embedded API.
        let request_id = crate::observability::current_request_id();
        let event = AuditEvent {
            ts,
            user,
            action,
            object,
            allowed,
            txn_id,
            request_id: request_id.clone(),
        };
        if let Ok(mut line) = serde_json::to_vec(&event) {
            line.push(b'\n');
            let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
            // Best-effort: an audit-write failure must not fail the operation,
            // but it is logged (a persistently failing audit sink is an ops
            // alert via the tracing pipeline).
            if let Err(e) = f.write_all(&line) {
                tracing::error!(error = %e, "audit log write failed");
            }
        }
        // Also mirror the decision as a structured `tracing` event so it lands
        // in the app log under the current request/txn span — the audit trail is
        // then retrievable by `request_id` from the app log too, not only from
        // `audit.log` (item 22, L2 correlation).
        tracing::info!(
            target: "unidb::audit",
            user,
            action,
            object,
            allowed,
            txn_id,
            request_id = request_id.as_deref(),
            "audit event"
        );
    }
}

/// Compile-time proof the audit log is shareable on the `Engine`.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<AuditLog>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn records_named_users_not_embedded() {
        let dir = tempdir().unwrap();
        let audit = AuditLog::open(dir.path()).unwrap();
        audit.record(Some("bob"), Some(7), "select", "accounts", true);
        audit.record(Some("bob"), Some(7), "insert", "accounts", false);
        audit.record(None, Some(7), "select", "accounts", true); // embedded → skipped
        audit.record_admin(None, None, "create_user", "carol", true); // admin → logged

        let contents = std::fs::read_to_string(dir.path().join("audit.log")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3, "embedded read is not audited; the rest are");
        assert!(lines[0].contains("\"user\":\"bob\"") && lines[0].contains("\"allowed\":true"));
        assert!(
            lines[0].contains("\"txn_id\":7"),
            "txn_id correlation is written"
        );
        assert!(lines[1].contains("\"allowed\":false"));
        assert!(lines[2].contains("create_user") && lines[2].contains("<embedded>"));
    }

    #[test]
    fn appends_across_reopen() {
        let dir = tempdir().unwrap();
        {
            let a = AuditLog::open(dir.path()).unwrap();
            a.record(Some("x"), Some(1), "select", "t", true);
        }
        let a = AuditLog::open(dir.path()).unwrap();
        a.record(Some("y"), Some(2), "delete", "t", true);
        let contents = std::fs::read_to_string(dir.path().join("audit.log")).unwrap();
        assert_eq!(contents.lines().count(), 2);
    }
}
