// Replication (P6.b): replication slots + WAL shipping.
//
// A **replication slot** is a named, persisted position (`restart_lsn`) that the
// primary must not truncate the WAL past — it guarantees a consumer (a read
// replica, an archiver, a change-data-feed) can always fetch every record it has
// not yet consumed, even across a checkpoint. The checkpoint's WAL truncation
// floor becomes `min(checkpoint_lsn, min slot restart_lsn)` so no consumer's
// segments are deleted early. A slot that stops advancing pins the WAL and makes
// it grow without bound — the classic footgun — so slot lag is surfaced for
// monitoring (P6.g) rather than hidden.
//
// **WAL shipping** is the read side: `Wal::records_from` / `ship_from` serialize
// every record after a given LSN into a framed byte stream a replica can apply
// via redo (the consumer side lands in P6.c). This is the single-primary +
// read-replica model of the roadmap — async by default, with an optional
// synchronous slot kind (P6.c) so a failover loses no acknowledged commit.
//
// Slots live in a small `slots.json` file next to the data (serde is fine here —
// this is config/metadata, not the page/WAL hot path, per CLAUDE.md §4). The
// registry is `Send + Sync` (interior `Mutex`) so it can sit on the shared
// `Engine`.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{DbError, Result},
    format::{Lsn, PageId, Xid},
};

mod replica;
pub use replica::Replica;

/// The primary control-file state a replica must adopt to interpret the shipped
/// WAL (P6.c). The catalog *content* rides the WAL (a `WAL_INSERT` on the
/// catalog page), but its **root pointer** and the transaction counter live in
/// the control file, which is not part of the WAL stream — so they travel
/// alongside it. Checkpoint LSN is deliberately *not* shipped: a replica always
/// replays its full local WAL from the start (materialize-from-clean), so it
/// keeps `checkpoint_lsn = INVALID`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrimaryControl {
    pub page_size: u32,
    pub catalog_root: PageId,
    pub next_xid: Xid,
}

/// How a slot's consumer is treated for commit durability.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlotKind {
    /// The primary streams to this consumer but never waits for it on commit
    /// (may lose the last un-shipped commits on failover — documented tradeoff).
    #[default]
    Async,
    /// A commit is not acknowledged until this consumer has confirmed the commit
    /// LSN (P6.c) — zero acknowledged-commit loss on failover.
    Sync,
}

/// A slot's public view (returned to callers + the REST layer).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotInfo {
    pub name: String,
    /// The WAL must be retained from here (inclusive). The consumer has
    /// confirmed everything strictly before it.
    pub restart_lsn: Lsn,
    pub kind: SlotKind,
}

/// The on-disk record for one slot (name is the map key).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SlotState {
    restart_lsn: Lsn,
    kind: SlotKind,
}

/// Persisted registry of replication slots. Every mutation atomically rewrites
/// `slots.json` (write-tmp + rename) so a crash never leaves a torn slot file.
pub struct SlotRegistry {
    path: PathBuf,
    inner: Mutex<BTreeMap<String, SlotState>>,
}

impl SlotRegistry {
    /// Open (or start empty) the slot registry rooted at `slots.json` inside
    /// `dir`. A missing or unreadable file starts empty (logged, not fatal).
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join("slots.json");
        let inner = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<BTreeMap<String, SlotState>>(&bytes)
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "slots.json unreadable — starting with no slots");
                    BTreeMap::new()
                }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, SlotState>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn persist(&self, map: &BTreeMap<String, SlotState>) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(map)
            .map_err(|e| DbError::Replication(format!("serialize slots: {e}")))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Create a new slot starting at `start_lsn` (typically the current WAL
    /// tail). Errors if a slot with this name already exists.
    pub fn create(&self, name: &str, start_lsn: Lsn, kind: SlotKind) -> Result<SlotInfo> {
        let mut map = self.lock();
        if map.contains_key(name) {
            return Err(DbError::Replication(format!(
                "slot '{name}' already exists"
            )));
        }
        map.insert(
            name.to_string(),
            SlotState {
                restart_lsn: start_lsn,
                kind,
            },
        );
        self.persist(&map)?;
        tracing::info!(slot = name, start_lsn, ?kind, "replication slot created");
        Ok(SlotInfo {
            name: name.to_string(),
            restart_lsn: start_lsn,
            kind,
        })
    }

    /// Drop a slot, releasing its WAL retention. Errors if unknown.
    pub fn drop_slot(&self, name: &str) -> Result<()> {
        let mut map = self.lock();
        if map.remove(name).is_none() {
            return Err(DbError::Replication(format!("slot '{name}' not found")));
        }
        self.persist(&map)?;
        tracing::info!(slot = name, "replication slot dropped");
        Ok(())
    }

    /// Advance a slot's `restart_lsn` to `lsn` (monotonic — a stale/duplicate
    /// confirmation never rewinds retention). Errors if the slot is unknown.
    pub fn advance(&self, name: &str, lsn: Lsn) -> Result<()> {
        let mut map = self.lock();
        let slot = map
            .get_mut(name)
            .ok_or_else(|| DbError::Replication(format!("slot '{name}' not found")))?;
        if lsn > slot.restart_lsn {
            slot.restart_lsn = lsn;
            self.persist(&map)?;
            tracing::debug!(slot = name, restart_lsn = lsn, "replication slot advanced");
        }
        Ok(())
    }

    /// The minimum `restart_lsn` across all slots — the WAL retention floor. None
    /// when there are no slots (the checkpoint may truncate freely).
    pub fn min_restart_lsn(&self) -> Option<Lsn> {
        self.lock().values().map(|s| s.restart_lsn).min()
    }

    /// True if any slot is synchronous (P6.c commit-wait).
    pub fn has_sync(&self) -> bool {
        self.lock().values().any(|s| s.kind == SlotKind::Sync)
    }

    /// Snapshot of every slot, sorted by name.
    pub fn list(&self) -> Vec<SlotInfo> {
        self.lock()
            .iter()
            .map(|(name, s)| SlotInfo {
                name: name.clone(),
                restart_lsn: s.restart_lsn,
                kind: s.kind,
            })
            .collect()
    }

    /// Look up one slot.
    pub fn get(&self, name: &str) -> Option<SlotInfo> {
        self.lock().get(name).map(|s| SlotInfo {
            name: name.to_string(),
            restart_lsn: s.restart_lsn,
            kind: s.kind,
        })
    }
}

/// Compile-time proof the registry is shareable on the `Engine`.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SlotRegistry>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn create_list_and_min() {
        let dir = tempdir().unwrap();
        let reg = SlotRegistry::open(dir.path()).unwrap();
        assert_eq!(reg.min_restart_lsn(), None);
        reg.create("replica_a", 10, SlotKind::Async).unwrap();
        reg.create("replica_b", 25, SlotKind::Sync).unwrap();
        assert_eq!(reg.min_restart_lsn(), Some(10));
        assert!(reg.has_sync());
        let list = reg.list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "replica_a");
    }

    #[test]
    fn duplicate_create_errors() {
        let dir = tempdir().unwrap();
        let reg = SlotRegistry::open(dir.path()).unwrap();
        reg.create("s", 1, SlotKind::Async).unwrap();
        assert!(reg.create("s", 2, SlotKind::Async).is_err());
    }

    #[test]
    fn advance_is_monotonic() {
        let dir = tempdir().unwrap();
        let reg = SlotRegistry::open(dir.path()).unwrap();
        reg.create("s", 5, SlotKind::Async).unwrap();
        reg.advance("s", 10).unwrap();
        assert_eq!(reg.get("s").unwrap().restart_lsn, 10);
        // A stale confirmation must not rewind retention.
        reg.advance("s", 7).unwrap();
        assert_eq!(reg.get("s").unwrap().restart_lsn, 10);
    }

    #[test]
    fn slots_persist_across_reopen() {
        let dir = tempdir().unwrap();
        {
            let reg = SlotRegistry::open(dir.path()).unwrap();
            reg.create("keep", 42, SlotKind::Sync).unwrap();
        }
        let reg = SlotRegistry::open(dir.path()).unwrap();
        let s = reg.get("keep").unwrap();
        assert_eq!(s.restart_lsn, 42);
        assert_eq!(s.kind, SlotKind::Sync);
    }

    #[test]
    fn drop_unknown_errors() {
        let dir = tempdir().unwrap();
        let reg = SlotRegistry::open(dir.path()).unwrap();
        assert!(reg.drop_slot("nope").is_err());
    }
}
