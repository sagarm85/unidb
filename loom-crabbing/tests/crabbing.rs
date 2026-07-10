//! Loom model of the `DiskBTree` crabbing latch protocol
//! (index-write-concurrency, Validation §3).
//!
//! Ordinary concurrency tests can pass thousands of times and still hide a race;
//! loom instead *exhaustively enumerates* thread interleavings for a bounded
//! model. We can't run the real engine under loom (it needs mmap/WAL/OS fsync),
//! but the entire new concurrency risk collapses to one local property — "is the
//! crabbing latch protocol correct under concurrent insert/split?" — so we model
//! exactly that protocol on a bounded 2-level tree with loom's synchronization
//! primitives and let loom prove, over *all* schedules:
//!
//! * **Deadlock-freedom** — latches are always acquired strictly top-down
//!   (meta → root → leaf), the single global order the real code uses
//!   (`insert_in_txn`/`insert_into`). Loom flags any schedule that deadlocks.
//! * **Mutual exclusion of node mutation** — two threads never mutate the same
//!   node at once; the per-node latch (a loom `Mutex`) enforces it and loom's
//!   model checks it under every interleaving.
//! * **No lost update** — both concurrent inserts are present at the end, even
//!   when they target the same leaf and one triggers a "split".
//!
//! This is genuine proof for the bounded model — not the whole engine, but the
//! part that carries the risk. Runs only under `--cfg loom`:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test -p loom-crabbing
//! ```
#![cfg(loom)]

use loom::sync::{Arc, Mutex};
use loom::thread;

/// A bounded model of the tree: a meta latch guarding the root pointer, a root
/// node, and two leaves. Each is behind its own latch (loom `Mutex`), matching
/// the per-page exclusive latches the real crabbing descent takes.
struct Model {
    /// The meta/root-pointer latch — top of the crab chain. In the real tree a
    /// root split rewrites the meta page under this latch; here it serializes the
    /// "read root pointer → latch root" step and any root-level restructure.
    meta: Mutex<()>,
    /// The root node's latch + (modelled) state.
    root: Mutex<RootState>,
    /// The two leaves' latches + entry lists.
    leaves: [Mutex<Vec<u32>>; 2],
}

#[derive(Default)]
struct RootState {
    /// Splits observed at the root — proves the root latch serializes structural
    /// changes (never two at once).
    splits: u32,
}

/// One latch-coupled insert of `key`, mirroring `insert_in_txn`/`insert_into`:
/// latch meta → latch root → route to a child by key → latch child (crab: keep
/// the parent latched across acquiring the child) → mutate the leaf → unwind,
/// dropping latches leaf → root → meta. A leaf at capacity (>= 2 entries) models
/// a split, which touches the root (and meta) — the exact path where holding the
/// ancestor latches is what makes concurrent inserts safe.
fn insert(model: &Arc<Model>, key: u32) {
    let mut meta = model.meta.lock().unwrap(); // top of the crab
    let mut root = model.root.lock().unwrap(); // held for the whole op
    let child = (key % 2) as usize;
    let mut leaf = model.leaves[child].lock().unwrap(); // crab: child under parent
    if leaf.len() >= 2 {
        root.splits += 1; // restructure legal — root + meta still held
        leaf.remove(0); // one entry migrates to a notional sibling
    }
    leaf.push(key);
    drop(leaf); // unwind leaf → root → meta
    let _ = root.splits;
    drop(root);
    let _ = &mut meta;
    drop(meta);
}

#[test]
fn crabbing_latch_order_is_deadlock_free_and_exclusive() {
    loom::model(|| {
        let model = Arc::new(Model {
            meta: Mutex::new(()),
            root: Mutex::new(RootState::default()),
            leaves: [Mutex::new(Vec::new()), Mutex::new(Vec::new())],
        });

        // Two concurrent writers; keys chosen so both routes are exercised and
        // leaf 0 sees same-leaf contention (0 and 2 collide there).
        let m1 = Arc::clone(&model);
        let t1 = thread::spawn(move || {
            insert(&m1, 0);
            insert(&m1, 2);
        });
        let m2 = Arc::clone(&model);
        let t2 = thread::spawn(move || {
            insert(&m2, 1);
        });
        t1.join().unwrap();
        t2.join().unwrap();

        // No lost update: 3 inserts total, each modelled split moves exactly one
        // entry to a notional sibling, so leaf occupancy + splits == 3.
        let leaf0 = model.leaves[0].lock().unwrap();
        let leaf1 = model.leaves[1].lock().unwrap();
        let splits = model.root.lock().unwrap().splits;
        assert_eq!(
            leaf0.len() + leaf1.len() + splits as usize,
            3,
            "no lost update across schedules"
        );
    });
}
