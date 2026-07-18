// Durable, paged, WAL-logged B+tree secondary index (P3.a — Phase 3, the moat).
//
// Replaces the M6 in-memory `BTreeMap`-backed index. Every node is a page in
// the shared page store, buffer-pool-managed, and every structural mutation is
// WAL-logged as a full node-page image (`WAL_INDEX`, redo-only) so the tree is
// **crash-recovered, never rebuilt on open** — the Phase-3 gate. `Engine::open`
// no longer scans the heap to reconstruct this index; it just reads the tree's
// stable meta page (whose id is recorded in the catalog per indexed column).
//
// ## On-disk shape
//
// A tree is a set of pages, all carrying the standard 28-byte page header
// (page_id / page_type / crc / lsn — so the buffer pool's CRC + D5 machinery
// applies unchanged). Node payload lives in the body (offset 28 onward):
//
//   * **meta page** (stable id, stored in the catalog, never changes):
//       body[0]      = NODE_META
//       body[1..5]   = root page id (u32) — updated in place when the root splits
//   * **leaf node**:
//       body[0]      = NODE_LEAF
//       body[1..3]   = entry_count (u16)
//       body[3..7]   = next_leaf page id (u32; INVALID_PAGE_ID = none) — right
//                      sibling link, so a range/duplicate scan walks leaves
//       body[7..]    = entry_count × (encoded key ‖ RowId)
//   * **internal node**:
//       body[0]      = NODE_INTERNAL
//       body[1..3]   = key_count (u16) — children = key_count + 1
//       body[3..7]   = children[0] (u32)
//       body[7..]    = key_count × (encoded key ‖ child page id (u32))
//
// Keys are the `Ord` projection of a `Literal` (`OrderedValue`); comparison is
// done in memory after decoding, so the byte encoding need not be
// order-preserving.
//
// ## Durability / recovery model
//
// Each `insert`/`remove` is one WAL mini-transaction bracketing every node page
// it touches (a leaf write, or a leaf+new-node+ancestors chain on a split, plus
// the meta page on a root split). Recovery redoes all pages of a committed
// mini-txn or none — atomic. There is **no undo**: a secondary-index entry is
// only ever a *hint*, re-validated against MVCC visibility downstream (see
// `sql/executor.rs::try_exec_select_btree`), so a stale/extra entry from an
// aborted or incomplete write is harmless. The one dangerous case — a
// committed, MVCC-visible heap row with no index entry (a false negative) — is
// prevented by ordering: the index mini-txn fsyncs during statement execution,
// before the surrounding user transaction reaches `WAL_TXN_COMMIT`, so any
// committed row's index entry is already durable.
//
// ## v1 simplifications (documented, not silent)
//
// * Deletes do not merge/rebalance underfull nodes — an emptied leaf stays
//   linked (wastes space, never wrong). Splits only ever grow the tree.
// * One mini-txn (one fsync) per key insert; `CREATE INDEX` backfill therefore
//   pays one fsync per row. Batching is a later perf item.
// * Pages freed by `DROP INDEX` leak until the FSM/large-object work reclaims
//   them, exactly like `DROP TABLE` heap pages today.
//
// ## Concurrency (index-write-concurrency, Item A)
//
// Under the concurrent-SQL-writes toggle, two writer threads can insert into the
// *same* tree at once (before, the SQL catalog write lock serialized them). The
// write paths are made race-safe by **latch coupling ("crabbing")** over the
// buffer pool's per-page exclusive latches (`latch_exclusive`, P5.a):
//
// * `insert` (`insert_in_txn`) descends latching each child before releasing the
//   parent, but drops **all** ancestor latches (and the meta latch) the moment it
//   reaches a node that is *safe* — one where adding a single entry cannot
//   overflow it, so it will not split and no ancestor can be modified
//   (`node_is_insert_safe`). The still-modifiable suffix of the path (the
//   `retained` frame stack) stays latched; a split propagates up through exactly
//   those nodes, and only a root split (root never released ⇒ meta still held)
//   repoints the meta page. Every node is read and rewritten under a stable
//   latch, so a concurrent insert can never observe or clobber a half-applied
//   split; latches are taken strictly root→leaf, so inserts cannot deadlock (a
//   single global order). Safe-node early release lets inserts into different
//   subtrees/leaves proceed in parallel (only same-leaf inserts and the brief
//   root/meta touch serialize) — that is what recovers the indexed concurrent-
//   write throughput toward the unindexed floor. The safe predicate is exact for
//   fixed-size (`Int`/`Bool`) keys; for variable-length `Text` keys it is
//   conservative (an internal node is never deemed safe, so more of the path is
//   held) — always correct, just less concurrent for text-keyed indexes.
// * `set_value`/`remove` (single-leaf rewrites, used by vacuum) locate the leaf
//   unlatched, then **re-read it under its exclusive latch** and recompute the
//   modification from the freshly-read bytes — so they never write back stale
//   pre-latch contents over a concurrent split.
// * Reads (`search_eq`/`search_range`/`find_leaf`/`max_entry`/`page_directory`)
//   stay **latch-free**: the buffer pool returns an owned per-page copy under its
//   mmap lock (no torn single-node read), the leaves are right-linked so a scan
//   that lands on a just-split leaf walks rightward to migrated keys, and every
//   returned RowId is only a *hint* re-validated against MVCC downstream — so a
//   transiently stale read is corrected, never wrong. Keeping reads unlatched
//   avoids readers blocking the writers whose throughput this change targets.
//
// Recovery is unchanged (A3): nodes stay full-page redo-only `WAL_INDEX` images
// and each insert is still one mini-txn — crabbing changes *who* writes a node,
// not *how* it recovers.

use crate::{
    bufferpool::{BufferPool, ExclusiveLatch, PageReader},
    error::{DbError, Result},
    format::{
        u16_from_le, u16_to_le, u32_from_le, u32_to_le, u64_from_le, u64_to_le, Lsn, PageId,
        INVALID_PAGE_ID, PAGE_TYPE_BTREE,
    },
    heap::RowId,
    page::{SlottedPage, PAGE_HEADER_SIZE},
    sql::logical::{CmpOp, Literal},
    wal::Wal,
};

/// A `Literal` projected down to the subset that's `Ord` — `Vector`/`Json`/
/// `Null` (and the P2 non-orderable types) have no meaningful total order for
/// indexing and are rejected at `CREATE INDEX` validation time
/// (`sql/executor.rs::exec_create_index`), so this conversion failing here
/// would indicate an upstream bug, not a normal runtime condition.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum OrderedValue {
    Int(i64),
    Text(String),
    Bool(bool),
}

impl TryFrom<&Literal> for OrderedValue {
    type Error = DbError;

    fn try_from(lit: &Literal) -> Result<Self> {
        match lit {
            Literal::Int(n) => Ok(OrderedValue::Int(*n)),
            Literal::Text(s) => Ok(OrderedValue::Text(s.clone())),
            Literal::Bool(b) => Ok(OrderedValue::Bool(*b)),
            other => Err(DbError::SqlUnsupported(format!(
                "{other:?} is not orderable for a BTree index"
            ))),
        }
    }
}

/// The four range comparators a B+tree can serve via a leaf walk. `Eq` is a
/// point lookup; `Ne` has no compact range representation and is intentionally
/// not representable here — see [`DiskBTree::search`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeOp {
    Lt,
    Le,
    Gt,
    Ge,
}

// Node-kind tags (body[0]).
const NODE_META: u8 = 0;
const NODE_LEAF: u8 = 1;
const NODE_INTERNAL: u8 = 2;

// Key encoding tags.
const KEY_INT: u8 = 0;
const KEY_TEXT: u8 = 1;
const KEY_BOOL: u8 = 2;

const ROWID_LEN: usize = 6; // page_id(4) + slot(2)

fn body_capacity(page_size: usize) -> usize {
    page_size - PAGE_HEADER_SIZE
}

fn encode_key(k: &OrderedValue, out: &mut Vec<u8>) {
    match k {
        OrderedValue::Int(n) => {
            out.push(KEY_INT);
            out.extend_from_slice(&u64_to_le(*n as u64));
        }
        OrderedValue::Text(s) => {
            out.push(KEY_TEXT);
            let bytes = s.as_bytes();
            out.extend_from_slice(&u16_to_le(bytes.len() as u16));
            out.extend_from_slice(bytes);
        }
        OrderedValue::Bool(b) => {
            out.push(KEY_BOOL);
            out.push(*b as u8);
        }
    }
}

/// Decode a key at `buf[pos..]`, returning the value and the new position.
fn decode_key(buf: &[u8], pos: usize) -> Result<(OrderedValue, usize)> {
    let corrupt = || DbError::Recovery("corrupt B+tree key".into());
    let tag = *buf.get(pos).ok_or_else(corrupt)?;
    match tag {
        KEY_INT => {
            let end = pos + 1 + 8;
            let arr: [u8; 8] = buf
                .get(pos + 1..end)
                .ok_or_else(corrupt)?
                .try_into()
                .unwrap();
            Ok((OrderedValue::Int(u64_from_le(arr) as i64), end))
        }
        KEY_TEXT => {
            let len_arr: [u8; 2] = buf
                .get(pos + 1..pos + 3)
                .ok_or_else(corrupt)?
                .try_into()
                .unwrap();
            let len = u16_from_le(len_arr) as usize;
            let start = pos + 3;
            let end = start + len;
            let s = std::str::from_utf8(buf.get(start..end).ok_or_else(corrupt)?)
                .map_err(|_| corrupt())?
                .to_string();
            Ok((OrderedValue::Text(s), end))
        }
        KEY_BOOL => {
            let b = *buf.get(pos + 1).ok_or_else(corrupt)?;
            Ok((OrderedValue::Bool(b != 0), pos + 2))
        }
        _ => Err(corrupt()),
    }
}

fn encoded_key_len(k: &OrderedValue) -> usize {
    match k {
        OrderedValue::Int(_) => 1 + 8,
        OrderedValue::Text(s) => 1 + 2 + s.len(),
        OrderedValue::Bool(_) => 1 + 1,
    }
}

fn encode_rowid(r: RowId, out: &mut Vec<u8>) {
    out.extend_from_slice(&u32_to_le(r.page_id));
    out.extend_from_slice(&u16_to_le(r.slot));
}

fn decode_rowid(buf: &[u8], pos: usize) -> Result<RowId> {
    let corrupt = || DbError::Recovery("corrupt B+tree RowId".into());
    let pg: [u8; 4] = buf
        .get(pos..pos + 4)
        .ok_or_else(corrupt)?
        .try_into()
        .unwrap();
    let sl: [u8; 2] = buf
        .get(pos + 4..pos + 6)
        .ok_or_else(corrupt)?
        .try_into()
        .unwrap();
    Ok(RowId {
        page_id: u32_from_le(pg),
        slot: u16_from_le(sl),
    })
}

/// Total order on `RowId` so equal-key entries have a stable secondary sort.
fn rowid_key(r: RowId) -> (PageId, u16) {
    (r.page_id, r.slot)
}

enum Node {
    Leaf {
        entries: Vec<(OrderedValue, RowId)>,
        next: PageId,
    },
    Internal {
        keys: Vec<OrderedValue>,
        children: Vec<PageId>,
    },
}

impl Node {
    /// Serialized body size (excluding the 28-byte page header).
    fn body_len(&self) -> usize {
        match self {
            Node::Leaf { entries, .. } => {
                7 + entries
                    .iter()
                    .map(|(k, _)| encoded_key_len(k) + ROWID_LEN)
                    .sum::<usize>()
            }
            Node::Internal { keys, children } => {
                let _ = children;
                7 + keys.iter().map(|k| encoded_key_len(k) + 4).sum::<usize>()
            }
        }
    }

    fn serialize(&self, page_id: PageId, page_size: usize) -> Vec<u8> {
        let mut buf = vec![0u8; page_size];
        buf[0..4].copy_from_slice(&u32_to_le(page_id));
        buf[4] = PAGE_TYPE_BTREE;
        let body = &mut buf[PAGE_HEADER_SIZE..];
        match self {
            Node::Leaf { entries, next } => {
                body[0] = NODE_LEAF;
                body[1..3].copy_from_slice(&u16_to_le(entries.len() as u16));
                body[3..7].copy_from_slice(&u32_to_le(*next));
                let mut payload = Vec::new();
                for (k, rid) in entries {
                    encode_key(k, &mut payload);
                    encode_rowid(*rid, &mut payload);
                }
                body[7..7 + payload.len()].copy_from_slice(&payload);
            }
            Node::Internal { keys, children } => {
                body[0] = NODE_INTERNAL;
                body[1..3].copy_from_slice(&u16_to_le(keys.len() as u16));
                body[3..7].copy_from_slice(&u32_to_le(children[0]));
                let mut payload = Vec::new();
                for (i, k) in keys.iter().enumerate() {
                    encode_key(k, &mut payload);
                    payload.extend_from_slice(&u32_to_le(children[i + 1]));
                }
                body[7..7 + payload.len()].copy_from_slice(&payload);
            }
        }
        buf
    }

    fn deserialize(page: &SlottedPage) -> Result<Node> {
        let raw = page.as_bytes();
        let body = &raw[PAGE_HEADER_SIZE..];
        let corrupt = || DbError::Recovery("corrupt B+tree node".into());
        match *body.first().ok_or_else(corrupt)? {
            NODE_LEAF => {
                let count = u16_from_le(body[1..3].try_into().unwrap()) as usize;
                let next = u32_from_le(body[3..7].try_into().unwrap());
                let mut pos = 7;
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let (k, np) = decode_key(body, pos)?;
                    let rid = decode_rowid(body, np)?;
                    pos = np + ROWID_LEN;
                    entries.push((k, rid));
                }
                Ok(Node::Leaf { entries, next })
            }
            NODE_INTERNAL => {
                let count = u16_from_le(body[1..3].try_into().unwrap()) as usize;
                let child0 = u32_from_le(body[3..7].try_into().unwrap());
                let mut children = Vec::with_capacity(count + 1);
                children.push(child0);
                let mut keys = Vec::with_capacity(count);
                let mut pos = 7;
                for _ in 0..count {
                    let (k, np) = decode_key(body, pos)?;
                    let child = u32_from_le(
                        body.get(np..np + 4)
                            .ok_or_else(corrupt)?
                            .try_into()
                            .unwrap(),
                    );
                    pos = np + 4;
                    keys.push(k);
                    children.push(child);
                }
                Ok(Node::Internal { keys, children })
            }
            _ => Err(corrupt()),
        }
    }
}

/// One node on the crabbing insert descent (index-write-concurrency, Item A):
/// its page id, the exclusive latch we hold on it, an owned snapshot of its
/// contents (read under that latch), and — for an internal node — the child
/// index we routed through (so a separator propagated up from a child split is
/// inserted at the right position). Held in the `retained` stack, which keeps
/// only the contiguous suffix of the path that may still be modified.
struct DescentFrame {
    pid: PageId,
    latch: ExclusiveLatch,
    node: Node,
    route_idx: usize,
}

/// Which child an *insert* of `value` routes to in an internal node's `keys`.
/// Uses strict `<` (not `<=`, which the read-path `find_leaf` uses): a new
/// duplicate key appends *after* existing ones, so an inserter descends into the
/// rightmost subtree whose separator is `<= value`. Matches the pre-crabbing
/// recursive routing exactly.
fn route_insert_child(keys: &[OrderedValue], value: &OrderedValue) -> usize {
    for (i, k) in keys.iter().enumerate() {
        if value < k {
            return i;
        }
    }
    keys.len()
}

/// Whether inserting one entry for `value` into `node` cannot overflow it — so
/// the node will not split and (transitively) no ancestor can be modified by
/// this insert. Used for **safe-node early release** during the crabbing descent:
/// on the first safe node we drop all ancestor + meta latches.
///
/// * **Leaf:** exact — we know the entry being added is `(value, rid)`, so the
///   growth is `encoded_key_len(value) + ROWID_LEN`.
/// * **Internal:** a child split pushes up one `(separator, child_ptr)` entry.
///   The separator is an existing key of the tree, so its size is the tree's key
///   type's size. For fixed-size key types (`Int`/`Bool`) that is exact; for
///   variable-length `Text` we cannot cheaply bound it, so we conservatively
///   report **unsafe** (the node is retained, never released early) — always
///   correct, just less concurrent for text-keyed indexes.
fn node_is_insert_safe(node: &Node, value: &OrderedValue, cap: usize) -> bool {
    match node {
        Node::Leaf { .. } => node.body_len() + encoded_key_len(value) + ROWID_LEN <= cap,
        Node::Internal { keys, .. } => match keys.first() {
            // child pointer is 4 bytes; key is fixed-size for Int/Bool.
            Some(OrderedValue::Int(_)) => {
                node.body_len() + encoded_key_len(&OrderedValue::Int(0)) + 4 <= cap
            }
            Some(OrderedValue::Bool(_)) => {
                node.body_len() + encoded_key_len(&OrderedValue::Bool(false)) + 4 <= cap
            }
            // Text keys (variable length) or an empty node → conservative.
            _ => false,
        },
    }
}

/// A handle to one durable B+tree, identified by its stable meta page id. It is
/// reconstructed on demand from the catalog (like `Heap::from_pages`), holding
/// no in-memory tree state — everything lives in buffer-pool-managed pages.
pub struct DiskBTree {
    meta_page: PageId,
    page_size: usize,
}

impl DiskBTree {
    pub fn new(meta_page: PageId, page_size: usize) -> Self {
        Self {
            meta_page,
            page_size,
        }
    }

    pub fn meta_page(&self) -> PageId {
        self.meta_page
    }

    /// Create a fresh empty tree: allocate a meta page and an empty leaf root,
    /// WAL-log both in one mini-txn, and return a handle. The caller records
    /// [`Self::meta_page`] durably in the catalog.
    pub fn create(pool: &BufferPool, wal: &Wal) -> Result<DiskBTree> {
        let meta_page = pool.alloc_page()?;
        let root_page = pool.alloc_page()?;
        let page_size = pool.page_size();

        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let root = Node::Leaf {
            entries: Vec::new(),
            next: INVALID_PAGE_ID,
        };
        let l1 = write_node(pool, wal, txn_id, begin_lsn, root_page, &root, page_size)?;
        let meta = meta_bytes(meta_page, root_page, page_size);
        let l2 = write_raw(pool, wal, txn_id, l1, meta_page, meta)?;
        wal.commit_mini_txn(txn_id, l2)?;
        Ok(DiskBTree::new(meta_page, page_size))
    }

    fn root_page(&self, pool: &BufferPool) -> Result<PageId> {
        let page = pool.fetch_page(self.meta_page)?;
        let body = &page.as_bytes()[PAGE_HEADER_SIZE..];
        if body.first().copied() != Some(NODE_META) {
            pool.unpin(self.meta_page);
            return Err(DbError::Recovery(format!(
                "B+tree meta page {} is not a meta node",
                self.meta_page
            )));
        }
        let root = u32_from_le(body[1..5].try_into().unwrap());
        pool.unpin(self.meta_page);
        Ok(root)
    }

    // ── reads ────────────────────────────────────────────────────────────────

    /// Dispatch a `CmpOp` to the right lookup. Returns `None` for `Ne` (no
    /// compact range representation) — the caller must treat `None` as "this
    /// index can't help, fall back to a full scan," never as "zero candidates."
    pub fn search(
        &self,
        op: CmpOp,
        value: &OrderedValue,
        pool: &BufferPool,
    ) -> Result<Option<Vec<RowId>>> {
        match op {
            CmpOp::Eq => Ok(Some(self.search_eq(value, pool)?)),
            CmpOp::Lt => Ok(Some(self.search_range(RangeOp::Lt, value, pool)?)),
            CmpOp::Le => Ok(Some(self.search_range(RangeOp::Le, value, pool)?)),
            CmpOp::Gt => Ok(Some(self.search_range(RangeOp::Gt, value, pool)?)),
            CmpOp::Ge => Ok(Some(self.search_range(RangeOp::Ge, value, pool)?)),
            CmpOp::Ne => Ok(None),
        }
    }

    /// Descend to the **leftmost** leaf that could contain `key`, following
    /// internal separators. Routing goes left on `key <= separator` (not `<`):
    /// a separator is the first key of its right subtree, so when `key` equals a
    /// separator the *first* occurrence of `key` may be an earlier duplicate in
    /// the left subtree. Reads then walk rightward via the leaf links from here,
    /// so a duplicate run straddling any number of leaf boundaries is fully
    /// collected. (The insert path has its own routing — it deliberately keeps
    /// `<` so new duplicates append after existing ones; only reads need the
    /// leftmost leaf.)
    fn find_leaf(&self, key: &OrderedValue, pool: &BufferPool) -> Result<PageId> {
        let mut pid = self.root_page(pool)?;
        loop {
            let page = pool.fetch_page(pid)?;
            let node = Node::deserialize(&page)?;
            pool.unpin(pid);
            match node {
                Node::Leaf { .. } => return Ok(pid),
                Node::Internal { keys, children } => {
                    let mut idx = keys.len();
                    for (i, k) in keys.iter().enumerate() {
                        if key <= k {
                            idx = i;
                            break;
                        }
                    }
                    pid = children[idx];
                }
            }
        }
    }

    /// Exact-match candidates. Starts at the leftmost leaf that could contain
    /// `value` (see [`Self::find_leaf`]) and walks rightward across the leaf
    /// links, collecting every `== value` entry, until it sees a key strictly
    /// greater than `value` (the run is over) or runs off the end. This is
    /// robust to a duplicate run straddling any number of leaf boundaries — the
    /// case that previously under-returned when a heavily-duplicated key (a
    /// full-text token in many docs, a graph hub, a BTree value on many rows)
    /// spanned a leaf split.
    pub fn search_eq(&self, value: &OrderedValue, pool: &BufferPool) -> Result<Vec<RowId>> {
        let mut pid = self.find_leaf(value, pool)?;
        let mut out = Vec::new();
        loop {
            let page = pool.fetch_page(pid)?;
            let node = Node::deserialize(&page)?;
            pool.unpin(pid);
            let Node::Leaf { entries, next } = node else {
                break;
            };
            let mut past = false;
            for (k, rid) in &entries {
                match k.cmp(value) {
                    std::cmp::Ordering::Less => {}
                    std::cmp::Ordering::Equal => out.push(*rid),
                    std::cmp::Ordering::Greater => {
                        past = true;
                        break;
                    }
                }
            }
            // Continue to the next leaf unless we've passed `value` (a key
            // greater than it appeared) — the run may span leaves, and starting
            // leaf may even be entirely `< value` if `find_leaf` landed early.
            if past || next == INVALID_PAGE_ID {
                break;
            }
            pid = next;
        }
        Ok(out)
    }

    /// Range-match candidates. `Lt`/`Le` scan from the leftmost leaf; `Gt`/`Ge`
    /// descend to the first qualifying leaf; both walk rightward via the leaf
    /// links, collecting every entry the predicate admits.
    pub fn search_range(
        &self,
        op: RangeOp,
        value: &OrderedValue,
        pool: &BufferPool,
    ) -> Result<Vec<RowId>> {
        let start = match op {
            RangeOp::Lt | RangeOp::Le => self.leftmost_leaf(pool)?,
            RangeOp::Gt | RangeOp::Ge => self.find_leaf(value, pool)?,
        };
        let mut pid = start;
        let mut out = Vec::new();
        loop {
            let page = pool.fetch_page(pid)?;
            let node = Node::deserialize(&page)?;
            pool.unpin(pid);
            let Node::Leaf { entries, next } = node else {
                break;
            };
            let mut stop = false;
            for (k, rid) in &entries {
                let admit = match op {
                    RangeOp::Lt => k < value,
                    RangeOp::Le => k <= value,
                    RangeOp::Gt => k > value,
                    RangeOp::Ge => k >= value,
                };
                if admit {
                    out.push(*rid);
                } else if matches!(op, RangeOp::Lt | RangeOp::Le) && k >= value {
                    // Ascending scan passed the upper bound — done entirely.
                    stop = true;
                    break;
                }
            }
            if stop || next == INVALID_PAGE_ID {
                break;
            }
            pid = next;
        }
        Ok(out)
    }

    /// Like [`search_range`] but stops after collecting `limit` `RowId`s,
    /// giving O(log n + limit) time rather than O(log n + all_past_cursor).
    /// Used by `poll_events`/`poll_events_after` so a consumer that is nearly
    /// caught up pays only O(log n + batch_size) even on a 1M-row table.
    pub fn search_range_limit(
        &self,
        op: RangeOp,
        value: &OrderedValue,
        limit: usize,
        pool: &BufferPool,
    ) -> Result<Vec<RowId>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let start = match op {
            RangeOp::Lt | RangeOp::Le => self.leftmost_leaf(pool)?,
            RangeOp::Gt | RangeOp::Ge => self.find_leaf(value, pool)?,
        };
        let mut pid = start;
        let mut out = Vec::with_capacity(limit);
        'outer: loop {
            let page = pool.fetch_page(pid)?;
            let node = Node::deserialize(&page)?;
            pool.unpin(pid);
            let Node::Leaf { entries, next } = node else {
                break;
            };
            for (k, rid) in &entries {
                let admit = match op {
                    RangeOp::Lt => k < value,
                    RangeOp::Le => k <= value,
                    RangeOp::Gt => k > value,
                    RangeOp::Ge => k >= value,
                };
                if admit {
                    out.push(*rid);
                    if out.len() >= limit {
                        break 'outer;
                    }
                } else if matches!(op, RangeOp::Lt | RangeOp::Le) && k >= value {
                    break 'outer;
                }
            }
            if next == INVALID_PAGE_ID {
                break;
            }
            pid = next;
        }
        Ok(out)
    }

    /// Partition the qualifying range into `n` approximately equal slices and
    /// return the collected `RowId`s pre-grouped by contiguous leaf-page run.
    /// Each partition covers a contiguous key range; callers dispatch one
    /// partition per worker for static (non-work-stealing) heap resolution,
    /// giving each worker a focused heap-page footprint compared with the
    /// interleaved access of a work-stealing cursor over a flat list.
    ///
    /// Used by `try_exec_select_btree` (item 45 Lever 1).
    pub fn search_range_partition(
        &self,
        op: RangeOp,
        value: &OrderedValue,
        n: usize,
        pool: &BufferPool,
    ) -> Result<Vec<Vec<RowId>>> {
        if n <= 1 {
            return Ok(vec![self.search_range(op, value, pool)?]);
        }

        // Walk the leaf chain with the same admittance logic as `search_range`,
        // collecting per-leaf RowId slices instead of one flat Vec.
        let start = match op {
            RangeOp::Lt | RangeOp::Le => self.leftmost_leaf(pool)?,
            RangeOp::Gt | RangeOp::Ge => self.find_leaf(value, pool)?,
        };
        let mut leaf_slices: Vec<Vec<RowId>> = Vec::new();
        let mut total = 0usize;
        let mut pid = start;
        let mut done = false;
        while !done {
            let page = pool.fetch_page(pid)?;
            let node = Node::deserialize(&page)?;
            pool.unpin(pid);
            let Node::Leaf { entries, next } = node else {
                break;
            };
            let mut leaf_rids: Vec<RowId> = Vec::new();
            for (k, rid) in &entries {
                let admit = match op {
                    RangeOp::Lt => k < value,
                    RangeOp::Le => k <= value,
                    RangeOp::Gt => k > value,
                    RangeOp::Ge => k >= value,
                };
                if admit {
                    leaf_rids.push(*rid);
                } else if matches!(op, RangeOp::Lt | RangeOp::Le) && k >= value {
                    done = true;
                    break;
                }
            }
            total += leaf_rids.len();
            if !leaf_rids.is_empty() {
                leaf_slices.push(leaf_rids);
            }
            if done || next == INVALID_PAGE_ID {
                break;
            }
            pid = next;
        }

        if total == 0 {
            let mut out = Vec::with_capacity(n);
            out.resize_with(n, Vec::new);
            return Ok(out);
        }

        // Distribute into exactly `n` partitions by entry count, using leaf
        // boundaries as natural split points so each partition is a contiguous
        // key run. Overflow entries accumulate in the final partition.
        let per_part = total.div_ceil(n);
        let mut partitions: Vec<Vec<RowId>> = Vec::with_capacity(n);
        let mut current: Vec<RowId> = Vec::with_capacity(per_part);
        let mut in_current = 0usize;
        for leaf_rids in leaf_slices {
            for rid in leaf_rids {
                current.push(rid);
                in_current += 1;
                if in_current >= per_part && partitions.len() + 1 < n {
                    partitions.push(std::mem::take(&mut current));
                    current = Vec::with_capacity(per_part);
                    in_current = 0;
                }
            }
        }
        if !current.is_empty() {
            partitions.push(current);
        }
        // Pad with empty slices so caller always gets exactly `n` entries.
        partitions.resize_with(n, Vec::new);
        Ok(partitions)
    }

    fn leftmost_leaf(&self, pool: &BufferPool) -> Result<PageId> {
        let mut pid = self.root_page(pool)?;
        loop {
            let page = pool.fetch_page(pid)?;
            let node = Node::deserialize(&page)?;
            pool.unpin(pid);
            match node {
                Node::Leaf { .. } => return Ok(pid),
                Node::Internal { children, .. } => pid = children[0],
            }
        }
    }

    fn rightmost_leaf(&self, pool: &BufferPool) -> Result<PageId> {
        let mut pid = self.root_page(pool)?;
        loop {
            let page = pool.fetch_page(pid)?;
            let node = Node::deserialize(&page)?;
            pool.unpin(pid);
            match node {
                Node::Leaf { .. } => return Ok(pid),
                // The last child holds every key >= the last separator, so it is
                // the rightmost subtree.
                Node::Internal { children, .. } => pid = children[children.len() - 1],
            }
        }
    }

    /// The single greatest `(key, rid)` entry, or `None` for an empty tree.
    /// O(tree height) — one root-to-rightmost-leaf descent, no full scan — so
    /// the durable FSM can find a heap's append tail (its highest page id)
    /// without the O(pages) directory walk `TableDef.pages` used to force.
    pub fn max_entry(&self, pool: &BufferPool) -> Result<Option<(OrderedValue, RowId)>> {
        let pid = self.rightmost_leaf(pool)?;
        let page = pool.fetch_page(pid)?;
        let node = Node::deserialize(&page)?;
        pool.unpin(pid);
        let Node::Leaf { entries, .. } = node else {
            return Err(DbError::Recovery(
                "B+tree rightmost node is not a leaf".into(),
            ));
        };
        Ok(entries.last().cloned())
    }

    /// Enumerate the durable FSM's `(page_id, free_bytes)` entries in ascending
    /// page-id order over **any** [`PageReader`] — the buffer pool *or* a
    /// concurrent reader's shared mmap ([`SharedPageReader`]). The FSM's keys are
    /// exactly the pages a heap owns (the directory) and each value carries that
    /// page's last-recorded free space (B2: encoded in the `RowId.slot` field,
    /// since a page's free space is `< page_size <= u16::MAX`), so this both
    /// reconstructs the directory a full scan/vacuum needs *and* warms the free
    /// map without re-fetching every heap page. Reader-only: no pin/unpin, no
    /// allocation, no WAL — never mutates the tree. O(pages), amortized into the
    /// O(pages) scan it feeds; never on the O(1)-open path.
    pub fn page_directory<P: PageReader>(&self, reader: &P) -> Result<Vec<(PageId, usize)>> {
        // Meta page → root (mirrors `root_page`, but over `read_page`).
        let meta = reader.read_page(self.meta_page)?;
        let mbody = &meta.as_bytes()[PAGE_HEADER_SIZE..];
        if mbody.first().copied() != Some(NODE_META) {
            return Err(DbError::Recovery(format!(
                "FSM meta page {} is not a meta node",
                self.meta_page
            )));
        }
        let mut pid = u32_from_le(mbody[1..5].try_into().unwrap());
        // Descend to the leftmost leaf.
        loop {
            let page = reader.read_page(pid)?;
            match Node::deserialize(&page)? {
                Node::Leaf { .. } => break,
                Node::Internal { children, .. } => pid = children[0],
            }
        }
        // Walk the leaf chain, collecting (page id from the key, free bytes from
        // the value's slot field).
        let mut out = Vec::new();
        loop {
            let page = reader.read_page(pid)?;
            let Node::Leaf { entries, next } = Node::deserialize(&page)? else {
                break;
            };
            for (k, rid) in &entries {
                match k {
                    OrderedValue::Int(n) => out.push((*n as PageId, rid.slot as usize)),
                    other => {
                        return Err(DbError::Recovery(format!(
                            "FSM key is not an Int page id: {other:?}"
                        )))
                    }
                }
            }
            if next == INVALID_PAGE_ID {
                break;
            }
            pid = next;
        }
        Ok(out)
    }

    // ── writes ─────────────────────────────────────────────────────────────────

    /// Insert `(value, rid)`. One WAL mini-txn covers every page touched,
    /// including any split chain and a root replacement. Duplicate keys are
    /// permitted (a value maps to many rows); an exact `(value, rid)` duplicate
    /// is a no-op.
    pub fn insert(
        &self,
        value: OrderedValue,
        rid: RowId,
        pool: &BufferPool,
        wal: &Wal,
    ) -> Result<()> {
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut prev_lsn = begin_lsn;
        self.insert_in_txn(value, rid, pool, wal, txn_id, &mut prev_lsn)?;
        wal.commit_mini_txn(txn_id, prev_lsn)?;
        Ok(())
    }

    /// In-place RowId patch for unchanged-key UPDATE (item 47, Phase A).
    ///
    /// When UPDATE changes a non-indexed column but leaves the indexed column's
    /// value the same, the key ordering in the leaf is preserved.  Instead of
    /// inserting a new `(key, new_rid)` alongside the existing `(key, old_rid)`
    /// (which fills leaves and causes splits → ≥2 page-image WAL records per
    /// row), this replaces `old_rid` with `new_rid` in-place for a single
    /// page-image WAL record and zero structural change.
    ///
    /// **Correctness / abort safety:** the caller MUST record a corresponding
    /// `UndoAction::BTreePatch { .. old_rid, new_rid .. }` so that if the
    /// enclosing user transaction aborts, `txn_mgr.abort()` restores `old_rid`
    /// by calling this method in reverse before clearing the xmax stamp.
    ///
    /// Falls back to a regular insert of `(key, new_rid)` if `(key, old_rid)`
    /// is not found (entry already cleaned up, or non-unique key).
    pub fn update_rowid_inplace(
        &self,
        key: OrderedValue,
        old_rid: RowId,
        new_rid: RowId,
        pool: &BufferPool,
        wal: &Wal,
    ) -> Result<()> {
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut prev_lsn = begin_lsn;

        let mut pid = self.find_leaf(&key, pool)?;
        let mut patched = false;
        'outer: loop {
            let page = pool.fetch_page(pid)?;
            let node = Node::deserialize(&page)?;
            pool.unpin(pid);
            let Node::Leaf { mut entries, next } = node else {
                break;
            };
            let mut modified = false;
            for (k, rid) in entries.iter_mut() {
                if *k > key {
                    break 'outer;
                }
                if *k == key && *rid == old_rid {
                    *rid = new_rid;
                    modified = true;
                    patched = true;
                    break;
                }
            }
            if modified {
                prev_lsn = write_node(
                    pool,
                    wal,
                    txn_id,
                    prev_lsn,
                    pid,
                    &Node::Leaf { entries, next },
                    self.page_size,
                )?;
                break;
            }
            if next == INVALID_PAGE_ID {
                break;
            }
            pid = next;
        }

        if !patched {
            self.insert_in_txn(key, new_rid, pool, wal, txn_id, &mut prev_lsn)?;
        }
        wal.commit_mini_txn(txn_id, prev_lsn)?;
        Ok(())
    }

    /// The body of [`Self::insert`], but participating in a **caller-supplied**
    /// mini-txn (`txn_id`, `prev_lsn`) instead of opening/committing its own.
    /// Lets a caller fold a tree insert into a larger atomic unit — the durable
    /// heap grow (`Heap::alloc_heap_page`) brackets the new page's init *and*
    /// its FSM directory entry in one mini-txn, so a crash mid-grow leaves
    /// neither (no orphan page, no torn directory) rather than a page absent
    /// from its directory (B2).
    pub fn insert_in_txn(
        &self,
        value: OrderedValue,
        rid: RowId,
        pool: &BufferPool,
        wal: &Wal,
        txn_id: u64,
        prev_lsn: &mut Lsn,
    ) -> Result<()> {
        // Latch-coupled ("crabbing") descent with **safe-node early release**
        // (index-write-concurrency, Item A). The meta page holds the root pointer;
        // a root split rewrites it, so it is the top of the latch chain and is
        // held while reading the root pointer + latching the root (atomic against
        // a concurrent root split). We then descend, latching each child before
        // releasing... — but crucially, the moment we latch a node that is *safe*
        // for this insert (adding one entry cannot overflow it, so it will not
        // split and no ancestor can be modified), we drop **all** ancestor latches
        // (and the meta latch): different inserts can then descend into different
        // subtrees fully in parallel. `retained` holds the contiguous suffix from
        // the highest node that might still be modified down to the current node,
        // each with its exclusive latch and the child index we routed through
        // (needed to place a propagated separator). Latches are always acquired
        // strictly top-down (meta → root → leaf), so concurrent inserts cannot
        // deadlock (A3). Reads stay latch-free (see `find_leaf`).
        let cap = body_capacity(self.page_size);
        // `meta_guard` is held until we prove the root will not be replaced (i.e.
        // we retained a safe node below the root, or the root itself is safe).
        let mut meta_guard = Some(pool.latch_exclusive(self.meta_page));
        let root = self.root_page(pool)?;
        let mut retained: Vec<DescentFrame> = Vec::new();
        retained.push(DescentFrame {
            pid: root,
            latch: pool.latch_exclusive(root),
            node: self.read_node(root, pool)?,
            route_idx: 0,
        });
        // Descend to the target leaf, releasing ancestors past each safe node.
        loop {
            let top = retained.last().unwrap();
            let Node::Internal { keys, children } = &top.node else {
                break; // reached the leaf
            };
            let idx = route_insert_child(keys, &value);
            let child = children[idx];
            retained.last_mut().unwrap().route_idx = idx;
            let child_latch = pool.latch_exclusive(child);
            let child_node = self.read_node(child, pool)?;
            if node_is_insert_safe(&child_node, &value, cap) {
                // Nothing at or above `child` can be modified by this insert →
                // release the meta latch and every retained ancestor.
                meta_guard = None;
                retained.clear();
            }
            retained.push(DescentFrame {
                pid: child,
                latch: child_latch,
                node: child_node,
                route_idx: 0,
            });
        }
        // Apply the insert at the leaf and propagate any split up through the
        // retained (still-latched) ancestors.
        let mut pending: Option<(OrderedValue, PageId)> = None;
        while let Some(frame) = retained.pop() {
            let _latch = frame.latch; // keep this node latched for its rewrite
            match frame.node {
                Node::Leaf { mut entries, next } => {
                    let probe = (value.clone(), rowid_key(rid));
                    let insert_pos = match entries
                        .binary_search_by(|(k, r)| (k.clone(), rowid_key(*r)).cmp(&probe))
                    {
                        Ok(_) => return Ok(()), // exact (key,rid) duplicate — no-op
                        Err(pos) => pos,
                    };
                    entries.insert(insert_pos, (value.clone(), rid));
                    let leaf = Node::Leaf { entries, next };
                    if leaf.body_len() <= cap {
                        // Non-split single insert: FPI + logical WAL record.
                        // frame.latch is still held (exclusive) on frame.pid —
                        // safe for maybe_log_fpi (P5.a / item 56 Step 4).
                        let _ = pool.fetch_page_for_write(frame.pid, wal)?;
                        if let Some(fpi_lsn) =
                            pool.maybe_log_fpi(frame.pid, wal, txn_id, *prev_lsn)?
                        {
                            *prev_lsn = fpi_lsn;
                        }
                        let mut key_bytes = Vec::new();
                        encode_key(&value, &mut key_bytes);
                        let lsn = wal.log_index_insert(
                            txn_id,
                            *prev_lsn,
                            frame.pid,
                            insert_pos as u16,
                            &key_bytes,
                            rid.page_id,
                            rid.slot,
                        )?;
                        let image = leaf.serialize(frame.pid, self.page_size);
                        let mut sp = SlottedPage::from_bytes_unchecked(image);
                        sp.set_lsn(lsn);
                        pool.write_page(&sp)?;
                        pool.unpin(frame.pid);
                        *prev_lsn = lsn;
                        return Ok(()); // absorbed — remaining ancestor latches drop
                    }
                    pending = Some(self.split_leaf(leaf, frame.pid, pool, wal, txn_id, prev_lsn)?);
                }
                Node::Internal {
                    mut keys,
                    mut children,
                } => {
                    let (sep_key, new_child) = pending.take().expect(
                        "an internal frame is only popped to absorb a split propagated from below",
                    );
                    keys.insert(frame.route_idx, sep_key);
                    children.insert(frame.route_idx + 1, new_child);
                    let internal = Node::Internal { keys, children };
                    if internal.body_len() <= cap {
                        *prev_lsn = write_node(
                            pool,
                            wal,
                            txn_id,
                            *prev_lsn,
                            frame.pid,
                            &internal,
                            self.page_size,
                        )?;
                        return Ok(()); // absorbed
                    }
                    pending = Some(
                        self.split_internal(internal, frame.pid, pool, wal, txn_id, prev_lsn)?,
                    );
                }
            }
        }
        // The topmost retained node split. It can only be the root (a safe node
        // absorbs and returns above), so the meta latch is still held — build a
        // new root over the two halves and repoint the meta page in this mini-txn.
        if let Some((sep_key, new_child)) = pending {
            debug_assert!(
                meta_guard.is_some(),
                "root split without the meta latch held — crabbing invariant violated"
            );
            let new_root_page = pool.alloc_page()?;
            let new_root = Node::Internal {
                keys: vec![sep_key],
                children: vec![root, new_child],
            };
            *prev_lsn = write_node(
                pool,
                wal,
                txn_id,
                *prev_lsn,
                new_root_page,
                &new_root,
                self.page_size,
            )?;
            let meta = meta_bytes(self.meta_page, new_root_page, self.page_size);
            *prev_lsn = write_raw(pool, wal, txn_id, *prev_lsn, self.meta_page, meta)?;
        }
        drop(meta_guard);
        Ok(())
    }

    /// Batch-insert `entries` in the caller's mini-txn, **coalescing WAL** so a
    /// leaf touched by many entries is logged **once** (a single full-page
    /// `WAL_INDEX` image carrying all of them) instead of once per entry — the
    /// A1 UPDATE index-maintenance win. A bulk UPDATE re-inserts each row's
    /// *existing* key as a new version, so thousands of entries land in a few
    /// dozen leaves; per-entry logging emits one ~8 KiB page image *per row*
    /// (RC2), while this emits one *per leaf touched*.
    ///
    /// Correctness is identical to calling [`Self::insert_in_txn`] once per
    /// entry: every `(key, rowid)` is inserted into the same sorted leaf, and a
    /// duplicate key spanning leaves is still fully collected by `search_eq`'s
    /// rightward walk. Only entries that fall inside a leaf's current
    /// `[min_key, max_key]` span **and** fit without a split are coalesced; a
    /// boundary/new key, an overflow (would split), or an unexpected shape falls
    /// back to the proven per-entry crabbing insert. The per-leaf exclusive
    /// latch is held across read-modify-write (re-reading under the latch, never
    /// clobbering a concurrent split with pre-latch bytes — the `set_value`
    /// discipline) and dropped before any fallback, so the crabbing path
    /// acquires its own latches top-down (no lock-order cycle, deadlock-free).
    pub fn insert_many_in_txn(
        &self,
        entries: &[(OrderedValue, RowId)],
        pool: &BufferPool,
        wal: &Wal,
        txn_id: u64,
        prev_lsn: &mut Lsn,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let cap = body_capacity(self.page_size);
        // Sort so entries destined for the same leaf are contiguous (a run).
        let mut sorted: Vec<(OrderedValue, RowId)> = entries.to_vec();
        sorted.sort_by_key(|(k, r)| (k.clone(), rowid_key(*r)));

        let mut i = 0usize;
        while i < sorted.len() {
            let leaf_pid = self.find_leaf(&sorted[i].0, pool)?;
            // Exclusive latch across read-modify-write; re-read under it so a
            // concurrent split's bytes are never clobbered.
            let latch = pool.latch_exclusive(leaf_pid);
            let node = self.read_node(leaf_pid, pool)?;
            let Node::Leaf { mut entries, next } = node else {
                // A find_leaf result should always be a leaf; be defensive.
                drop(latch);
                self.insert_in_txn(
                    sorted[i].0.clone(),
                    sorted[i].1,
                    pool,
                    wal,
                    txn_id,
                    prev_lsn,
                )?;
                i += 1;
                continue;
            };
            // Need a key span to test membership; an empty leaf is transient —
            // let the crabbing path handle it.
            let (Some(min_key), Some(max_key)) = (
                entries.first().map(|(k, _)| k.clone()),
                entries.last().map(|(k, _)| k.clone()),
            ) else {
                drop(latch);
                self.insert_in_txn(
                    sorted[i].0.clone(),
                    sorted[i].1,
                    pool,
                    wal,
                    txn_id,
                    prev_lsn,
                )?;
                i += 1;
                continue;
            };

            // Compute existing leaf body once; reused by both the item-84 proactive
            // check below and the absorption loop's capacity guard.
            let existing_body: usize = 7 + entries
                .iter()
                .map(|(k, _)| encoded_key_len(k) + ROWID_LEN)
                .sum::<usize>();

            // Item 84 — Proactive batch check.
            //
            // Count ALL entries in sorted[i..] that belong to [min_key, max_key].
            // If the combined body (existing leaf + all new entries) would overflow
            // the page AND there are ≥2 new entries, skip absorption entirely and
            // call insert_batch_in_txn. This replaces the two-phase
            // "absorb until full (1 WAL_INDEX) + split 1 overflow entry via
            // insert_in_txn (3 WAL_INDEX) = 4 WAL_INDEX" pattern with a single
            // balanced merge-split (3 WAL_INDEX) for the whole batch.
            //
            // The original absorbed==0 check only fired when the FIRST new entry
            // could not fit (leaf already at cap). Since item 84's own balanced
            // splits create half-full leaves (~272 entries), the typical workload
            // is: leaf has room → absorbs ~270 entries → fills → 1 overflow entry
            // triggers absorbed==0 with n=1 → absorbed==0 path ran insert_in_txn.
            // The proactive check catches the overflow BEFORE absorption begins.
            {
                let total_n = sorted[i..]
                    .iter()
                    .take_while(|(k, _)| k >= &min_key && k <= &max_key)
                    .count();
                if total_n >= 2 {
                    let new_body: usize = sorted[i..i + total_n]
                        .iter()
                        .map(|(k, _)| encoded_key_len(k) + ROWID_LEN)
                        .sum::<usize>();
                    if existing_body + new_body > cap {
                        // Would overflow: batch all total_n entries in one descent.
                        drop(latch);
                        let consumed = self.insert_batch_in_txn(
                            &sorted[i..i + total_n],
                            pool,
                            wal,
                            txn_id,
                            prev_lsn,
                        )?;
                        i += consumed;
                        continue;
                    }
                }
            }

            // Running body size so the fit check stays O(1) per entry.
            // Reuse already-computed existing_body to avoid scanning entries twice.
            let mut cur_body: usize = existing_body;
            let mut absorbed = 0usize;
            let mut j = i;
            while j < sorted.len() {
                let (k, r) = (sorted[j].0.clone(), sorted[j].1);
                // Only absorb keys this leaf definitively owns. A key past
                // `max_key` may belong to a right sibling (route via find_leaf).
                if k < min_key || k > max_key {
                    break;
                }
                let probe = (k.clone(), rowid_key(r));
                match entries.binary_search_by(|(ek, er)| (ek.clone(), rowid_key(*er)).cmp(&probe))
                {
                    Ok(_) => {
                        // exact (key,rid) duplicate — a no-op, matching
                        // insert_in_txn, but consumed by the batch.
                        j += 1;
                        absorbed += 1;
                        continue;
                    }
                    Err(pos) => {
                        let added = encoded_key_len(&k) + ROWID_LEN;
                        if cur_body + added > cap {
                            break; // would split — stop; the remainder falls through
                        }
                        entries.insert(pos, (k, r));
                        cur_body += added;
                    }
                }
                j += 1;
                absorbed += 1;
            }

            if absorbed == 0 {
                // Head entry couldn't be coalesced (leaf is full or boundary).
                // Count entries in range for the single-entry fallback path —
                // the proactive check above already handled the n≥2 overflow case.
                let n = sorted[i..]
                    .iter()
                    .take_while(|(k, _)| k >= &min_key && k <= &max_key)
                    .count();
                drop(latch);
                if n >= 2 {
                    // Reached here only when proactive check missed (combined body
                    // estimate was too conservative). Use batch path anyway.
                    let consumed = self.insert_batch_in_txn(
                        &sorted[i..i + n],
                        pool,
                        wal,
                        txn_id,
                        prev_lsn,
                    )?;
                    i += consumed;
                } else {
                    self.insert_in_txn(
                        sorted[i].0.clone(),
                        sorted[i].1,
                        pool,
                        wal,
                        txn_id,
                        prev_lsn,
                    )?;
                    i += 1;
                }
                continue;
            }

            // One WAL_INDEX image for every entry absorbed into this leaf.
            let leaf = Node::Leaf { entries, next };
            *prev_lsn = write_node(
                pool,
                wal,
                txn_id,
                *prev_lsn,
                leaf_pid,
                &leaf,
                self.page_size,
            )?;
            drop(latch);
            i = j;
        }
        Ok(())
    }

    /// [`Self::insert_many_in_txn`] wrapped in its own mini-txn.
    pub fn insert_many(
        &self,
        entries: &[(OrderedValue, RowId)],
        pool: &BufferPool,
        wal: &Wal,
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut prev_lsn = begin_lsn;
        self.insert_many_in_txn(entries, pool, wal, txn_id, &mut prev_lsn)?;
        wal.commit_mini_txn(txn_id, prev_lsn)?;
        Ok(())
    }

    /// Item 84 — merge-split bulk path.
    ///
    /// `batch` is a pre-sorted (key-ascending) slice of entries that the caller
    /// knows belong to a *single* leaf's current key range. This function does one
    /// full crabbing descent to find that leaf, merges all `batch` entries into the
    /// leaf (duplicate `(key, rid)` pairs are silently skipped), then:
    ///
    /// * if the combined entries fit in one page: writes the leaf in-place (1 WAL
    ///   `WAL_INDEX` record), returns the number of entries consumed.
    /// * if they overflow a single page: does a **balanced** 2-way split, writes
    ///   both halves (2 `WAL_INDEX`), propagates the separator to the parent via
    ///   the crabbing retained-ancestor stack (1 more `WAL_INDEX` for the parent),
    ///   and returns the entries consumed.
    ///
    /// For entries that land outside the leaf's current range (rare: a concurrent
    /// split shrank the range between the caller's count and our descent), the
    /// function returns a smaller count; the caller then re-routes those via
    /// subsequent [`Self::insert_many_in_txn`] iterations.
    ///
    /// **Why this beats the default**: `insert_many_in_txn` normally absorbs new
    /// entries into a split leaf half until the half is exactly full (543 entries
    /// for Int keys on 8 KiB pages), then the very next entry triggers a second
    /// split (3 extra WAL_INDEX records). For a dense-overlap UPDATE (e.g.
    /// `SET k=k+1`), every interior leaf in the update range triggers this cascade:
    /// current = 8 WAL_INDEX/leaf, bulk-path = 3 WAL_INDEX/leaf — ~70% reduction
    /// in B-tree WAL per UPDATE non-HOT statement.
    fn insert_batch_in_txn(
        &self,
        batch: &[(OrderedValue, RowId)],
        pool: &BufferPool,
        wal: &Wal,
        txn_id: u64,
        prev_lsn: &mut Lsn,
    ) -> Result<usize> {
        debug_assert!(!batch.is_empty());
        let cap = body_capacity(self.page_size);

        // Crabbing descent — same as insert_in_txn but we do NOT apply the
        // safe-node early-release: we don't know yet whether the leaf will split,
        // so we hold the meta latch and every ancestor until we decide at the leaf.
        let meta_guard = pool.latch_exclusive(self.meta_page);
        let root = self.root_page(pool)?;
        let mut retained: Vec<DescentFrame> = Vec::new();
        retained.push(DescentFrame {
            pid: root,
            latch: pool.latch_exclusive(root),
            node: self.read_node(root, pool)?,
            route_idx: 0,
        });
        loop {
            let top = retained.last().unwrap();
            let Node::Internal { keys, children } = &top.node else {
                break; // reached a leaf
            };
            let idx = route_insert_child(keys, &batch[0].0);
            let child = children[idx];
            retained.last_mut().unwrap().route_idx = idx;
            retained.push(DescentFrame {
                pid: child,
                latch: pool.latch_exclusive(child),
                node: self.read_node(child, pool)?,
                route_idx: 0,
            });
        }

        // N = number of batch entries that actually belong to this leaf's range.
        // Re-check under the latch in case a concurrent split shrank the range.
        // Extract what we need from the leaf frame before any borrow issues arise.
        let (leaf_pid, old_entries, leaf_next) = {
            let frame = retained.last().unwrap();
            match &frame.node {
                Node::Leaf { entries, next } => (frame.pid, entries.clone(), *next),
                _ => {
                    // Unexpected: should always be a leaf after crabbing descent.
                    drop(retained);
                    drop(meta_guard);
                    self.insert_in_txn(
                        batch[0].0.clone(),
                        batch[0].1,
                        pool,
                        wal,
                        txn_id,
                        prev_lsn,
                    )?;
                    return Ok(1);
                }
            }
        };

        // Empty leaf is a transient state; fall back to single insert.
        let (min_key, max_key) = match (
            old_entries.first().map(|(k, _)| k),
            old_entries.last().map(|(k, _)| k),
        ) {
            (Some(lo), Some(hi)) => (lo.clone(), hi.clone()),
            _ => {
                drop(retained);
                drop(meta_guard);
                self.insert_in_txn(
                    batch[0].0.clone(),
                    batch[0].1,
                    pool,
                    wal,
                    txn_id,
                    prev_lsn,
                )?;
                return Ok(1);
            }
        };

        // Count entries from batch that still fall in [min_key, max_key].
        let n = batch
            .iter()
            .take_while(|(k, _)| k >= &min_key && k <= &max_key)
            .count();

        if n == 0 {
            // Range changed (concurrent split). Caller will re-route.
            drop(retained);
            drop(meta_guard);
            return Ok(0);
        }
        let n_consumed = n;

        // O(N+M) linear merge of two pre-sorted slices.
        // Both old_entries (leaf) and batch[..n] are sorted by (key, rowid_key(rid)).
        // The prior O(N²) Vec::insert loop caused ~295k element-shifts per leaf
        // (543 inserts × 543 elements shifted) → 10-15× timing regression vs
        // the correct merge; this version is O(N+M) with no shifting.
        let old = old_entries;
        let new_entries = &batch[..n];
        let mut combined: Vec<(OrderedValue, RowId)> = Vec::with_capacity(old.len() + n);
        let mut oi = 0usize;
        let mut bi = 0usize;
        while oi < old.len() && bi < n {
            let cmp = {
                let ok = (&old[oi].0, rowid_key(old[oi].1));
                let bk = (&new_entries[bi].0, rowid_key(new_entries[bi].1));
                ok.cmp(&bk)
            };
            match cmp {
                std::cmp::Ordering::Less => {
                    combined.push(old[oi].clone());
                    oi += 1;
                }
                std::cmp::Ordering::Equal => {
                    // Exact (key, rid) duplicate — keep old entry, skip new.
                    combined.push(old[oi].clone());
                    oi += 1;
                    bi += 1;
                }
                std::cmp::Ordering::Greater => {
                    combined.push((new_entries[bi].0.clone(), new_entries[bi].1));
                    bi += 1;
                }
            }
        }
        combined.extend_from_slice(&old[oi..]);
        for entry in &new_entries[bi..] {
            combined.push((entry.0.clone(), entry.1));
        }

        let combined_body: usize = 7 + combined
            .iter()
            .map(|(k, _)| encoded_key_len(k) + ROWID_LEN)
            .sum::<usize>();

        // Decide: fits in one leaf, needs a 2-way split, or too large to batch.
        let mut pending: Option<(OrderedValue, PageId)>;
        if combined_body <= cap {
            // All fit in one leaf: write in-place, no split.
            // The leaf latch is still held via `retained.last()`.
            let leaf = Node::Leaf {
                entries: combined,
                next: leaf_next,
            };
            *prev_lsn = write_node(pool, wal, txn_id, *prev_lsn, leaf_pid, &leaf, self.page_size)?;
            // No separator to propagate; drop retained (releases latch).
            drop(retained);
            drop(meta_guard);
            return Ok(n_consumed);
        } else if combined_body <= 2 * cap {
            // Balanced 2-way split: both halves fill to ≤ cap.
            let mid = combined.len() / 2;
            let right_entries = combined[mid..].to_vec();
            let left_entries = combined[..mid].to_vec();
            let sep_key = right_entries[0].0.clone();
            let right_page = pool.alloc_page()?;
            let right = Node::Leaf { entries: right_entries, next: leaf_next };
            let left  = Node::Leaf { entries: left_entries,  next: right_page };
            *prev_lsn = write_node(pool, wal, txn_id, *prev_lsn, right_page, &right, self.page_size)?;
            *prev_lsn = write_node(pool, wal, txn_id, *prev_lsn, leaf_pid,   &left,  self.page_size)?;
            pending = Some((sep_key, right_page));
        } else {
            // combined > 2 pages: more than 2 leaves needed (rare: >1086 entries
            // for one leaf range with Int keys).  Fall back to single-entry path.
            drop(retained);
            drop(meta_guard);
            self.insert_in_txn(batch[0].0.clone(), batch[0].1, pool, wal, txn_id, prev_lsn)?;
            return Ok(1);
        }

        // Pop the leaf frame (latch drops when frame is dropped).
        retained.pop();

        // Propagate split separator up through the retained ancestor stack,
        // exactly as insert_in_txn does.
        while let Some(frame) = retained.pop() {
            let _latch = frame.latch;
            let Node::Internal { mut keys, mut children } = frame.node else {
                unreachable!("non-leaf in ancestor retained stack")
            };
            let Some((sep_key, new_child)) = pending.take() else {
                // No split to propagate — release the rest of the stack.
                break;
            };
            keys.insert(frame.route_idx, sep_key);
            children.insert(frame.route_idx + 1, new_child);
            let internal = Node::Internal { keys, children };
            if internal.body_len() <= cap {
                *prev_lsn = write_node(pool, wal, txn_id, *prev_lsn, frame.pid, &internal, self.page_size)?;
                pending = None;
                break; // absorbed; remaining ancestors unchanged
            }
            pending = Some(self.split_internal(internal, frame.pid, pool, wal, txn_id, prev_lsn)?);
        }

        // If the root itself split, create a new root (same as insert_in_txn).
        // meta_guard is still held here (we hold it for the whole function so that
        // a root split — which rewrites the meta page — is atomic).
        if let Some((sep_key, new_child)) = pending {
            let root_pid = self.root_page(pool)?; // current root before the new root is written
            let new_root_page = pool.alloc_page()?;
            let new_root = Node::Internal {
                keys: vec![sep_key],
                children: vec![root_pid, new_child],
            };
            *prev_lsn = write_node(pool, wal, txn_id, *prev_lsn, new_root_page, &new_root, self.page_size)?;
            let meta = meta_bytes(self.meta_page, new_root_page, self.page_size);
            *prev_lsn = write_raw(pool, wal, txn_id, *prev_lsn, self.meta_page, meta)?;
        }
        drop(meta_guard);
        Ok(n_consumed)
    }

    /// Batch in-place RowId patch (item 47, Phase A).  Mirrors the coalescing
    /// behaviour of [`Self::insert_many_in_txn`]: patches for the same leaf are
    /// gathered and written as **one WAL page-image** instead of one per row.
    /// `patches` is `(key, old_rid, new_rid)` — for each entry, `(key, old_rid)`
    /// is located in the leaf and its RowId is overwritten with `new_rid`.
    ///
    /// Entries whose `(key, old_rid)` is not found in the leaf (already cleaned
    /// up or a non-unique key collision) fall back to a regular insert of
    /// `(key, new_rid)` so the new row version remains reachable.
    pub fn patch_many(
        &self,
        patches: &[(OrderedValue, RowId, RowId)],
        pool: &BufferPool,
        wal: &Wal,
    ) -> Result<()> {
        if patches.is_empty() {
            return Ok(());
        }
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut prev_lsn = begin_lsn;
        // Sort so patches for the same leaf are contiguous.
        let mut sorted = patches.to_vec();
        sorted.sort_by_key(|(k, old_r, _)| (k.clone(), rowid_key(*old_r)));

        let mut i = 0usize;
        while i < sorted.len() {
            let leaf_pid = self.find_leaf(&sorted[i].0, pool)?;
            let latch = pool.latch_exclusive(leaf_pid);
            let node = self.read_node(leaf_pid, pool)?;
            let Node::Leaf { mut entries, next } = node else {
                drop(latch);
                let (ref k, _, new_rid) = sorted[i];
                self.insert_in_txn(k.clone(), new_rid, pool, wal, txn_id, &mut prev_lsn)?;
                i += 1;
                continue;
            };
            let (Some(min_key), Some(max_key)) = (
                entries.first().map(|(k, _)| k.clone()),
                entries.last().map(|(k, _)| k.clone()),
            ) else {
                drop(latch);
                let (ref k, _, new_rid) = sorted[i];
                self.insert_in_txn(k.clone(), new_rid, pool, wal, txn_id, &mut prev_lsn)?;
                i += 1;
                continue;
            };

            let mut modified = false;
            let mut fallbacks: Vec<(OrderedValue, RowId)> = Vec::new();
            let mut j = i;
            loop {
                // The min/max bounds check only gates whether ADDITIONAL
                // (j > i) patches from the sorted batch also belong to this
                // leaf, so it can never fire on the very first entry (j ==
                // i) -- `find_leaf` is what put us on this page for that
                // key, but a leaf's *current* entries don't have to span its
                // full structural key range (e.g. right after a split), so
                // `sorted[i].0` can legitimately fall outside
                // `entries.first()/last()`. Gating on the bounds check for
                // j == i would `break` before `j` ever advances, leaving
                // `i = j` a no-op and looping forever on the same index.
                // Always processing j == i here (falling back to
                // `insert_in_txn` below if the exact (key, old_rid) entry
                // isn't in this leaf, exactly like any other not-found case)
                // guarantees `j` advances past `i` every iteration.
                let (ref pk, pold, pnew) = sorted[j];
                if j > i && (pk < &min_key || pk > &max_key) {
                    break;
                }
                // Find the specific (pk, pold) entry and patch its RowId.
                match entries.iter_mut().find(|(k, r)| k == pk && *r == pold) {
                    Some((_, rid)) => {
                        *rid = pnew;
                        modified = true;
                    }
                    None => fallbacks.push((pk.clone(), pnew)),
                }
                j += 1;
                if j >= sorted.len() {
                    break;
                }
            }
            if modified {
                prev_lsn = write_node(
                    pool,
                    wal,
                    txn_id,
                    prev_lsn,
                    leaf_pid,
                    &Node::Leaf { entries, next },
                    self.page_size,
                )?;
            }
            drop(latch);
            // Fallback inserts for patches whose old entry was not found.
            for (k, new_r) in fallbacks {
                self.insert_in_txn(k, new_r, pool, wal, txn_id, &mut prev_lsn)?;
            }
            i = j;
        }
        wal.commit_mini_txn(txn_id, prev_lsn)?;
        Ok(())
    }

    /// Fetch + deserialize one node while the caller already holds its exclusive
    /// latch (Item A). The pin is dropped immediately; the caller's latch keeps
    /// the bytes stable (no other writer can be mid-mutation of a latched node),
    /// so the returned owned `Node` is a consistent snapshot to modify and write
    /// back under the same latch.
    fn read_node(&self, pid: PageId, pool: &BufferPool) -> Result<Node> {
        let page = pool.fetch_page(pid)?;
        let node = Node::deserialize(&page)?;
        pool.unpin(pid);
        Ok(node)
    }

    /// Split a full leaf `pid` in half: the right half moves to a fresh page and
    /// becomes `pid`'s right sibling (`next_leaf`), both written in `txn_id`.
    /// Returns the separator + new page for the parent to absorb.
    fn split_leaf(
        &self,
        leaf: Node,
        pid: PageId,
        pool: &BufferPool,
        wal: &Wal,
        txn_id: u64,
        prev_lsn: &mut Lsn,
    ) -> Result<(OrderedValue, PageId)> {
        let Node::Leaf { entries, next } = leaf else {
            unreachable!()
        };
        let mid = entries.len() / 2;
        let right_entries = entries[mid..].to_vec();
        let left_entries = entries[..mid].to_vec();
        let sep_key = right_entries[0].0.clone();
        let right_page = pool.alloc_page()?;
        let right = Node::Leaf {
            entries: right_entries,
            next,
        };
        let left = Node::Leaf {
            entries: left_entries,
            next: right_page,
        };
        *prev_lsn = write_node(
            pool,
            wal,
            txn_id,
            *prev_lsn,
            right_page,
            &right,
            self.page_size,
        )?;
        *prev_lsn = write_node(pool, wal, txn_id, *prev_lsn, pid, &left, self.page_size)?;
        Ok((sep_key, right_page))
    }

    /// Split a full internal node `pid`: the middle key rises to the parent, the
    /// right half of keys/children moves to a fresh page. Returns the rising
    /// separator + new page.
    fn split_internal(
        &self,
        internal: Node,
        pid: PageId,
        pool: &BufferPool,
        wal: &Wal,
        txn_id: u64,
        prev_lsn: &mut Lsn,
    ) -> Result<(OrderedValue, PageId)> {
        let Node::Internal { keys, children } = internal else {
            unreachable!()
        };
        let mid = keys.len() / 2;
        let up_key = keys[mid].clone();
        let left_keys = keys[..mid].to_vec();
        let right_keys = keys[mid + 1..].to_vec();
        let left_children = children[..=mid].to_vec();
        let right_children = children[mid + 1..].to_vec();
        let right_page = pool.alloc_page()?;
        let right = Node::Internal {
            keys: right_keys,
            children: right_children,
        };
        let left = Node::Internal {
            keys: left_keys,
            children: left_children,
        };
        *prev_lsn = write_node(
            pool,
            wal,
            txn_id,
            *prev_lsn,
            right_page,
            &right,
            self.page_size,
        )?;
        *prev_lsn = write_node(pool, wal, txn_id, *prev_lsn, pid, &left, self.page_size)?;
        Ok((up_key, right_page))
    }

    /// Update the value (`RowId`) of the entry with key `value` in place, if it
    /// exists — the durable FSM uses this to record a page's new free space
    /// (`RowId.slot`) after a vacuum `compact_page` reclaims space, without
    /// adding a second entry for the same page. One WAL mini-txn, a single leaf
    /// rewrite, **no split** (the key set is unchanged, so the leaf size is
    /// unchanged). Returns `false` if the key is absent (the caller then falls
    /// back to `insert`). Keys in the FSM are unique (one entry per page), so at
    /// most one entry matches.
    pub fn set_value(
        &self,
        value: &OrderedValue,
        new_rid: RowId,
        pool: &BufferPool,
        wal: &Wal,
    ) -> Result<bool> {
        // Locate the leaf unlatched, then re-read it *under* its exclusive latch
        // (index-write-concurrency, Item A): a concurrent insert may have split
        // the leaf between locating and latching it, so the modification must be
        // computed from the latched-and-freshly-read node, never from bytes read
        // before the latch (that would clobber a concurrent split). If the key
        // migrated to the split's right sibling it is simply absent here and we
        // return `false` — the caller (vacuum's free-space update) treats a lost
        // update as a safe stale-low free hint, corrected by the next vacuum.
        let pid = self.find_leaf(value, pool)?;
        let _latch = pool.latch_exclusive(pid);
        let node = {
            let page = pool.fetch_page(pid)?;
            let n = Node::deserialize(&page)?;
            pool.unpin(pid);
            n
        };
        let Node::Leaf { mut entries, next } = node else {
            return Ok(false);
        };
        let Some(slot) = entries.iter().position(|(k, _)| k == value) else {
            return Ok(false);
        };
        entries[slot].1 = new_rid;
        let leaf = Node::Leaf { entries, next };
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let lsn = write_node(pool, wal, txn_id, begin_lsn, pid, &leaf, self.page_size)?;
        wal.commit_mini_txn(txn_id, lsn)?;
        Ok(true)
    }

    /// Remove one `(value, rid)` entry if present. No rebalance (v1). One WAL
    /// mini-txn. A missing entry is a no-op — removal is best-effort tidiness
    /// (MVCC re-check already filters stale candidates); the M10 vacuum path
    /// calls this to keep reclaimed slots from lingering as candidates.
    pub fn remove(
        &self,
        value: &OrderedValue,
        rid: RowId,
        pool: &BufferPool,
        wal: &Wal,
    ) -> Result<()> {
        // Walk leaves rightward from the leftmost candidate leaf (a duplicate
        // run may span leaves, and `find_leaf` lands at-or-before the run's
        // start), stopping once we pass `value`.
        let mut pid = self.find_leaf(value, pool)?;
        loop {
            // Re-read each leaf *under* its exclusive latch (index-write-
            // concurrency, Item A) so the retain/rewrite is computed from current
            // bytes — never from a copy read before the latch, which a concurrent
            // insert/split could have superseded. Remove is best-effort (a missing
            // entry is a no-op; MVCC re-check filters stale candidates anyway), so
            // an entry that migrated across a concurrent split is simply skipped.
            let _latch = pool.latch_exclusive(pid);
            let node = {
                let page = pool.fetch_page(pid)?;
                let n = Node::deserialize(&page)?;
                pool.unpin(pid);
                n
            };
            let Node::Leaf { mut entries, next } = node else {
                return Ok(());
            };
            let past = entries.last().map(|(k, _)| k > value).unwrap_or(true);
            let before = entries.len();
            entries.retain(|(k, r)| !(k == value && *r == rid));
            if entries.len() != before {
                let leaf = Node::Leaf { entries, next };
                let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
                let lsn = write_node(pool, wal, txn_id, begin_lsn, pid, &leaf, self.page_size)?;
                wal.commit_mini_txn(txn_id, lsn)?;
                return Ok(());
            }
            if past || next == INVALID_PAGE_ID {
                return Ok(()); // not present
            }
            drop(_latch);
            pid = next;
        }
    }

    /// Structural validator (index-write-concurrency, Validation §1). Walks the
    /// whole tree over any [`PageReader`] and returns every `(key, rid)` entry in
    /// leaf-chain order, asserting the tree's structural invariants along the
    /// way — turning silent index corruption a concurrent-write race might cause
    /// into a hard, loud failure. Run it as the assertion at the end of every
    /// concurrency stress test. Checks:
    ///
    /// 1. the leaf chain is fully linked from the leftmost leaf and terminates;
    /// 2. entries are globally non-decreasing across the whole chain (each leaf
    ///    sorted, and every leaf's first key ≥ the previous leaf's last key);
    /// 3. every internal separator is consistent with the leaf ordering
    ///    (implied by (2) plus the descent landing on the leftmost leaf);
    /// 4. no cycle in the leaf chain (bounded by a visited-page guard).
    ///
    /// It does **not** validate MVCC visibility (entries are hints) — only that
    /// the B+tree itself is well-formed and lost/duplicated no entries.
    /// Returns the entries so a caller can additionally assert set-equality with
    /// what it inserted (no lost or duplicated `(key, rid)` pairs).
    pub fn validate<P: PageReader>(&self, reader: &P) -> Result<Vec<(OrderedValue, RowId)>> {
        // Descend to the leftmost leaf.
        let meta = reader.read_page(self.meta_page)?;
        let mbody = &meta.as_bytes()[PAGE_HEADER_SIZE..];
        if mbody.first().copied() != Some(NODE_META) {
            return Err(DbError::Recovery(
                "validate: meta page is not a meta node".into(),
            ));
        }
        let mut pid = u32_from_le(mbody[1..5].try_into().unwrap());
        loop {
            let page = reader.read_page(pid)?;
            match Node::deserialize(&page)? {
                Node::Leaf { .. } => break,
                Node::Internal { children, .. } => {
                    if children.is_empty() {
                        return Err(DbError::Recovery(
                            "validate: internal node with no children".into(),
                        ));
                    }
                    pid = children[0];
                }
            }
        }
        // Walk the leaf chain, collecting entries and checking global order.
        let mut out: Vec<(OrderedValue, RowId)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut last_key: Option<OrderedValue> = None;
        loop {
            if !seen.insert(pid) {
                return Err(DbError::Recovery(format!(
                    "validate: cycle in leaf chain at page {pid}"
                )));
            }
            let page = reader.read_page(pid)?;
            let Node::Leaf { entries, next } = Node::deserialize(&page)? else {
                return Err(DbError::Recovery(
                    "validate: leaf-chain walk hit a non-leaf".into(),
                ));
            };
            for (k, rid) in entries {
                if let Some(prev) = &last_key {
                    if &k < prev {
                        return Err(DbError::Recovery(format!(
                            "validate: leaf entries out of order ({prev:?} then {k:?}) at page {pid}"
                        )));
                    }
                }
                last_key = Some(k.clone());
                out.push((k, rid));
            }
            if next == INVALID_PAGE_ID {
                break;
            }
            pid = next;
        }
        Ok(out)
    }
}

/// Build a meta page's raw bytes (CRC filled by `write_raw`/the caller).
fn meta_bytes(meta_page: PageId, root_page: PageId, page_size: usize) -> Vec<u8> {
    let mut buf = vec![0u8; page_size];
    buf[0..4].copy_from_slice(&u32_to_le(meta_page));
    buf[4] = PAGE_TYPE_BTREE;
    let body = &mut buf[PAGE_HEADER_SIZE..];
    body[0] = NODE_META;
    body[1..5].copy_from_slice(&u32_to_le(root_page));
    buf
}

/// Serialize a node to a full page image, WAL-log it (`WAL_INDEX`, redo-only
/// full-page image), stamp the record LSN into the page, and write it into the
/// buffer pool. Returns the record LSN so the caller can chain `prev_lsn`.
fn write_node(
    pool: &BufferPool,
    wal: &Wal,
    txn_id: u64,
    prev_lsn: Lsn,
    page_id: PageId,
    node: &Node,
    page_size: usize,
) -> Result<Lsn> {
    let image = node.serialize(page_id, page_size);
    write_raw(pool, wal, txn_id, prev_lsn, page_id, image)
}

/// Like [`write_node`] but takes an already-serialized page image (used for the
/// meta page). Pins the page for write, logs the image, stamps the LSN, writes.
fn write_raw(
    pool: &BufferPool,
    wal: &Wal,
    txn_id: u64,
    prev_lsn: Lsn,
    page_id: PageId,
    image: Vec<u8>,
) -> Result<Lsn> {
    // Pin the page into a frame so the pool tracks it dirty and the checkpoint
    // flush will msync it (write_page only marks an *already-framed* page dirty).
    let _ = pool.fetch_page_for_write(page_id, wal)?;
    let lsn = wal.log_index(txn_id, prev_lsn, page_id, &image)?;
    let mut sp = SlottedPage::from_bytes_unchecked(image);
    sp.set_lsn(lsn); // stamps LSN + recomputes CRC
    pool.write_page(&sp)?;
    pool.unpin(page_id);
    // Item 84: WAL_INDEX already IS a full-page image, so this page's before-image
    // is covered for this checkpoint interval. Mark it so that any subsequent
    // log_index_insert (logical WAL) on the same page does not emit a redundant
    // WAL_FPI. This is safe: recovery redoes from the WAL_INDEX record if needed.
    pool.mark_fpi_logged(page_id);
    Ok(lsn)
}

/// Apply a WAL_INDEX_INSERT logical redo record to a B-tree leaf page.
/// Decodes the key from `key_bytes` (B-tree key encoding: type-tag byte +
/// data), inserts (key, rid) at leaf entry position `slot`, and returns the
/// re-serialized page with the entry inserted. Caller stamps the LSN and
/// calls `pool.write_page`. Used exclusively by `recovery::redo_record`.
pub fn redo_index_insert(
    page: &SlottedPage,
    slot: u16,
    key_bytes: &[u8],
    rid: RowId,
    page_size: usize,
) -> Result<SlottedPage> {
    let mut node = Node::deserialize(page)?;
    let Node::Leaf {
        ref mut entries, ..
    } = node
    else {
        return Err(DbError::Recovery(
            "WAL_INDEX_INSERT redo: target page is not a B-tree leaf node".into(),
        ));
    };
    let (key, _) = decode_key(key_bytes, 0)?;
    let pos = slot as usize;
    if pos > entries.len() {
        return Err(DbError::Recovery(format!(
            "WAL_INDEX_INSERT redo: slot {pos} out of range (leaf has {} entries)",
            entries.len()
        )));
    }
    entries.insert(pos, (key, rid));
    let image = node.serialize(page.page_id(), page_size);
    Ok(SlottedPage::from_bytes_unchecked(image))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{DEFAULT_PAGE_SIZE, INVALID_LSN};
    use tempfile::tempdir;

    fn rid(page: u32, slot: u16) -> RowId {
        RowId {
            page_id: page,
            slot,
        }
    }

    struct Env {
        pool: BufferPool,
        wal: Wal,
        _dir: tempfile::TempDir,
    }

    fn env() -> Env {
        let dir = tempdir().unwrap();
        let pool =
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 256).unwrap();
        let wal = Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap();
        Env {
            pool,
            wal,
            _dir: dir,
        }
    }

    #[test]
    fn ordered_value_rejects_non_orderable_literals() {
        assert!(OrderedValue::try_from(&Literal::Vector(vec![0.1])).is_err());
        assert!(OrderedValue::try_from(&Literal::Null).is_err());
        assert_eq!(
            OrderedValue::try_from(&Literal::Int(5)).unwrap(),
            OrderedValue::Int(5)
        );
    }

    #[test]
    fn max_entry_and_page_directory_span_leaves() {
        let e = env();
        let t = DiskBTree::create(&e.pool, &e.wal).unwrap();
        // Empty tree.
        assert_eq!(t.max_entry(&e.pool).unwrap(), None);
        assert!(t.page_directory(&e.pool).unwrap().is_empty());
        // Enough keys to force several leaf splits, so rightmost/leftmost
        // descent and the leaf-chain walk are actually exercised (not a single
        // leaf). Insert out of order to prove ordering is by key, not arrival.
        const N: i64 = 400;
        for i in (0..N).rev() {
            t.insert(OrderedValue::Int(i), rid(i as u32, 0), &e.pool, &e.wal)
                .unwrap();
        }
        // Tail = highest key (the heap's append point).
        assert_eq!(
            t.max_entry(&e.pool).unwrap(),
            Some((OrderedValue::Int(N - 1), rid((N - 1) as u32, 0)))
        );
        // Full directory enumeration, ascending, every page id present once —
        // over the pool, which is itself a `PageReader` (the mmap reader takes
        // the same path).
        let dir = t.page_directory(&e.pool).unwrap();
        assert_eq!(dir.len(), N as usize);
        for (i, (pid, _free)) in dir.iter().enumerate() {
            assert_eq!(*pid, i as u32);
        }
    }

    #[test]
    fn set_value_updates_in_place_without_duplicating() {
        let e = env();
        let t = DiskBTree::create(&e.pool, &e.wal).unwrap();
        // FSM-style: one entry per page key, value's slot carries free bytes.
        for pid in 0..50u32 {
            t.insert(
                OrderedValue::Int(pid as i64),
                rid(pid, 100),
                &e.pool,
                &e.wal,
            )
            .unwrap();
        }
        // Update page 7's "free" (slot) in place.
        let updated = t
            .set_value(&OrderedValue::Int(7), rid(7, 4096), &e.pool, &e.wal)
            .unwrap();
        assert!(updated);
        // Exactly one entry for key 7, now carrying the new value.
        let dir = t.page_directory(&e.pool).unwrap();
        let sevens: Vec<_> = dir.iter().filter(|(p, _)| *p == 7).collect();
        assert_eq!(sevens.len(), 1, "must not duplicate the key");
        assert_eq!(sevens[0].1, 4096);
        // A missing key reports false (caller falls back to insert).
        assert!(!t
            .set_value(&OrderedValue::Int(999), rid(999, 0), &e.pool, &e.wal)
            .unwrap());
    }

    #[test]
    fn insert_and_search_eq() {
        let e = env();
        let t = DiskBTree::create(&e.pool, &e.wal).unwrap();
        t.insert(OrderedValue::Int(5), rid(1, 0), &e.pool, &e.wal)
            .unwrap();
        t.insert(OrderedValue::Int(7), rid(2, 0), &e.pool, &e.wal)
            .unwrap();
        t.insert(OrderedValue::Int(5), rid(3, 0), &e.pool, &e.wal)
            .unwrap();
        let mut got = t.search_eq(&OrderedValue::Int(5), &e.pool).unwrap();
        got.sort_by_key(|r| r.page_id);
        assert_eq!(got, vec![rid(1, 0), rid(3, 0)]);
        assert!(t
            .search_eq(&OrderedValue::Int(99), &e.pool)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn range_queries() {
        let e = env();
        let t = DiskBTree::create(&e.pool, &e.wal).unwrap();
        for i in 1..=5 {
            t.insert(OrderedValue::Int(i), rid(i as u32, 0), &e.pool, &e.wal)
                .unwrap();
        }
        assert_eq!(
            t.search_range(RangeOp::Lt, &OrderedValue::Int(3), &e.pool)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            t.search_range(RangeOp::Le, &OrderedValue::Int(3), &e.pool)
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            t.search_range(RangeOp::Gt, &OrderedValue::Int(3), &e.pool)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            t.search_range(RangeOp::Ge, &OrderedValue::Int(3), &e.pool)
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn search_dispatches_and_rejects_ne() {
        let e = env();
        let t = DiskBTree::create(&e.pool, &e.wal).unwrap();
        t.insert(OrderedValue::Int(1), rid(1, 0), &e.pool, &e.wal)
            .unwrap();
        assert!(t
            .search(CmpOp::Eq, &OrderedValue::Int(1), &e.pool)
            .unwrap()
            .is_some());
        assert!(t
            .search(CmpOp::Lt, &OrderedValue::Int(1), &e.pool)
            .unwrap()
            .is_some());
        assert!(t
            .search(CmpOp::Ne, &OrderedValue::Int(1), &e.pool)
            .unwrap()
            .is_none());
    }

    #[test]
    fn remove_drops_entry() {
        let e = env();
        let t = DiskBTree::create(&e.pool, &e.wal).unwrap();
        t.insert(OrderedValue::Int(1), rid(1, 0), &e.pool, &e.wal)
            .unwrap();
        t.insert(OrderedValue::Int(1), rid(2, 0), &e.pool, &e.wal)
            .unwrap();
        t.remove(&OrderedValue::Int(1), rid(1, 0), &e.pool, &e.wal)
            .unwrap();
        assert_eq!(
            t.search_eq(&OrderedValue::Int(1), &e.pool).unwrap(),
            vec![rid(2, 0)]
        );
    }

    #[test]
    fn text_keys() {
        let e = env();
        let t = DiskBTree::create(&e.pool, &e.wal).unwrap();
        t.insert(
            OrderedValue::Text("banana".into()),
            rid(1, 0),
            &e.pool,
            &e.wal,
        )
        .unwrap();
        t.insert(
            OrderedValue::Text("apple".into()),
            rid(2, 0),
            &e.pool,
            &e.wal,
        )
        .unwrap();
        assert_eq!(
            t.search_eq(&OrderedValue::Text("apple".into()), &e.pool)
                .unwrap(),
            vec![rid(2, 0)]
        );
    }

    /// Force many splits (a multi-level tree) and confirm every key is still
    /// findable — the core correctness proof for node splitting + routing.
    #[test]
    fn many_inserts_force_splits_and_stay_searchable() {
        let e = env();
        let t = DiskBTree::create(&e.pool, &e.wal).unwrap();
        // Enough to force several leaf splits and at least one internal level
        // (~480 entries/leaf), proving split + separator routing. Kept modest
        // because each insert is its own fsyncing mini-txn.
        let n = 2000i64;
        for i in 0..n {
            t.insert(OrderedValue::Int(i), rid(i as u32, 0), &e.pool, &e.wal)
                .unwrap();
        }
        for i in 0..n {
            let got = t.search_eq(&OrderedValue::Int(i), &e.pool).unwrap();
            assert_eq!(got, vec![rid(i as u32, 0)], "key {i} missing after splits");
        }
        // Range over the whole set.
        assert_eq!(
            t.search_range(RangeOp::Ge, &OrderedValue::Int(0), &e.pool)
                .unwrap()
                .len(),
            n as usize
        );
    }

    /// Regression (found by the P3.c recall spike): a single key with enough
    /// duplicate `RowId`s to span several leaves must return **all** of them
    /// from `search_eq`. Before the leftmost-descent + walk-right fix,
    /// `find_leaf` could land mid-run and the walk stopped early, silently
    /// under-returning a heavily-duplicated key (a full-text token in many docs,
    /// a graph hub, a BTree value on many rows).
    #[test]
    fn heavily_duplicated_key_spanning_leaves_returns_all() {
        let e = env();
        let t = DiskBTree::create(&e.pool, &e.wal).unwrap();
        // Neighbours on both sides so the hot key's run sits between other keys
        // and its leaves split with real separators around it.
        let dup = 3000u32; // far more than one leaf holds (~480 int entries)
        for i in 0..500 {
            t.insert(OrderedValue::Int(1), rid(i, 0), &e.pool, &e.wal)
                .unwrap();
        }
        for i in 0..dup {
            t.insert(OrderedValue::Int(2), rid(i, 1), &e.pool, &e.wal)
                .unwrap();
        }
        for i in 0..500 {
            t.insert(OrderedValue::Int(3), rid(i, 2), &e.pool, &e.wal)
                .unwrap();
        }
        let got = t.search_eq(&OrderedValue::Int(2), &e.pool).unwrap();
        assert_eq!(got.len(), dup as usize, "must return every duplicate");
        assert!(got.iter().all(|r| r.slot == 1));
        // Neighbours unaffected.
        assert_eq!(
            t.search_eq(&OrderedValue::Int(1), &e.pool).unwrap().len(),
            500
        );
        assert_eq!(
            t.search_eq(&OrderedValue::Int(3), &e.pool).unwrap().len(),
            500
        );
        // Remove one from deep in the run, then re-count.
        t.remove(&OrderedValue::Int(2), rid(1500, 1), &e.pool, &e.wal)
            .unwrap();
        assert_eq!(
            t.search_eq(&OrderedValue::Int(2), &e.pool).unwrap().len(),
            dup as usize - 1
        );
    }

    /// Item A crabbing: N threads insert disjoint *and* overlapping key ranges
    /// into ONE shared tree at once; afterwards the structural validator must
    /// report a well-formed tree containing exactly the inserted set — no lost or
    /// duplicated entries, leaf chain sorted and linked. This is the module-level
    /// proof that the latch-coupled descent is race-safe (the whole point of
    /// Item A). Deterministic assertion (validator §1) over a nondeterministic
    /// schedule; run many keys to force splits under contention.
    #[test]
    fn concurrent_inserts_stay_structurally_valid() {
        use std::sync::Arc;
        let dir = tempdir().unwrap();
        let pool = Arc::new(
            BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 512).unwrap(),
        );
        let wal = Arc::new(Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap());
        let tree = Arc::new(DiskBTree::create(&pool, &wal).unwrap());

        let threads = 8;
        let per = 500i64;
        // Half the threads write a disjoint block; half write into a *shared*
        // overlapping range so many duplicate keys straddle leaf splits under
        // concurrency (the hardest case for the leaf-chain / split path).
        let barrier = Arc::new(std::sync::Barrier::new(threads));
        let mut handles = Vec::new();
        for t in 0..threads {
            let (tree, pool, wal, barrier) =
                (tree.clone(), pool.clone(), wal.clone(), barrier.clone());
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for i in 0..per {
                    let key = if t % 2 == 0 {
                        (t as i64) * per + i // disjoint block
                    } else {
                        i % 50 // shared overlapping range → heavy duplicates
                    };
                    tree.insert(OrderedValue::Int(key), rid(t as u32, i as u16), &pool, &wal)
                        .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        // Structural validity + exact set membership.
        let entries = tree.validate(&*pool).unwrap();
        let mut expected = std::collections::HashSet::new();
        for t in 0..threads {
            for i in 0..per {
                let key = if t % 2 == 0 {
                    (t as i64) * per + i
                } else {
                    i % 50
                };
                expected.insert((key, t as u32, i as u16));
            }
        }
        let got: std::collections::HashSet<_> = entries
            .iter()
            .map(|(k, r)| {
                let OrderedValue::Int(n) = k else { panic!() };
                (*n, r.page_id, r.slot)
            })
            .collect();
        assert_eq!(got.len(), entries.len(), "no duplicated (key,rid) entries");
        assert_eq!(got, expected, "every inserted entry present, none extra");
        // Every key still resolvable via the normal read path.
        for key in 0..50i64 {
            assert!(
                !tree
                    .search_eq(&OrderedValue::Int(key), &pool)
                    .unwrap()
                    .is_empty(),
                "shared key {key} missing after concurrent inserts"
            );
        }
    }

    /// Deterministic split-contention (index-write-concurrency, Validation §2).
    /// Pre-fill the tree right up to a leaf-split boundary, then release two
    /// threads *simultaneously* (a barrier) to insert into the hot region — so
    /// the schedule is forced onto the dangerous path where concurrent inserts
    /// race a leaf/root split. Because the crabbing descent holds the path's
    /// exclusive latches, one writer always completes its split before the other
    /// observes the node — the structural validator afterwards must find a
    /// well-formed tree with exactly the inserted set (no half-applied split, no
    /// lost/duplicated entry). Repeated a few times to vary the interleaving.
    #[test]
    fn two_writers_splitting_hot_region_stay_valid() {
        use std::sync::Arc;
        for round in 0..5u32 {
            let dir = tempdir().unwrap();
            let pool = Arc::new(
                BufferPool::open(&dir.path().join("data.db"), DEFAULT_PAGE_SIZE as usize, 256)
                    .unwrap(),
            );
            let wal = Arc::new(Wal::open(&dir.path().join("db.wal"), INVALID_LSN).unwrap());
            let tree = Arc::new(DiskBTree::create(&pool, &wal).unwrap());

            // Pre-fill enough int keys to build a multi-leaf tree, leaving the
            // top of the key space open for both threads to pile into the same
            // rightmost leaves and force splits there.
            let prefill = 900i64;
            for i in 0..prefill {
                tree.insert(OrderedValue::Int(i), rid(i as u32, 0), &pool, &wal)
                    .unwrap();
            }

            let barrier = Arc::new(std::sync::Barrier::new(2));
            let mut handles = Vec::new();
            for t in 0..2u32 {
                let (tree, pool, wal, barrier) =
                    (tree.clone(), pool.clone(), wal.clone(), barrier.clone());
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    // Interleave the two threads' keys in the hot (high) region so
                    // both drive splits of the same leaves.
                    for j in 0..200i64 {
                        let key = prefill + j * 2 + t as i64;
                        tree.insert(
                            OrderedValue::Int(key),
                            rid(1_000_000 + t, j as u16),
                            &pool,
                            &wal,
                        )
                        .unwrap();
                    }
                }));
            }
            for h in handles {
                h.join().unwrap();
            }

            let entries = tree.validate(&*pool).unwrap();
            let got: std::collections::BTreeSet<i64> = entries
                .iter()
                .map(|(k, _)| match k {
                    OrderedValue::Int(n) => *n,
                    _ => panic!(),
                })
                .collect();
            let mut expected: std::collections::BTreeSet<i64> = (0..prefill).collect();
            for t in 0..2i64 {
                for j in 0..200i64 {
                    expected.insert(prefill + j * 2 + t);
                }
            }
            assert_eq!(
                got, expected,
                "round {round}: split contention lost/added keys"
            );
            assert_eq!(
                entries.len(),
                expected.len(),
                "round {round}: duplicated entry"
            );
        }
    }

    /// The tree survives being reconstructed from just its meta page id — the
    /// Phase-3 "no rebuild on open" property, at the module level.
    #[test]
    fn reopen_from_meta_page_only() {
        let e = env();
        let meta = {
            let t = DiskBTree::create(&e.pool, &e.wal).unwrap();
            for i in 0..500i64 {
                t.insert(OrderedValue::Int(i), rid(i as u32, 0), &e.pool, &e.wal)
                    .unwrap();
            }
            t.meta_page()
        };
        // A brand-new handle over the same meta page — no rebuild, no scan.
        let t2 = DiskBTree::new(meta, DEFAULT_PAGE_SIZE as usize);
        assert_eq!(
            t2.search_eq(&OrderedValue::Int(250), &e.pool).unwrap(),
            vec![rid(250, 0)]
        );
    }
}
