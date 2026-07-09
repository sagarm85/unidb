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

use crate::{
    bufferpool::BufferPool,
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
        let root = self.root_page(pool)?;
        let (txn_id, begin_lsn) = wal.begin_mini_txn()?;
        let mut prev_lsn = begin_lsn;
        let split = self.insert_into(root, value, rid, pool, wal, txn_id, &mut prev_lsn)?;
        if let Some((sep_key, new_child)) = split {
            // Root split: build a new internal root over the two halves and
            // repoint the meta page — all in this same mini-txn.
            let new_root_page = pool.alloc_page()?;
            let new_root = Node::Internal {
                keys: vec![sep_key],
                children: vec![root, new_child],
            };
            prev_lsn = write_node(
                pool,
                wal,
                txn_id,
                prev_lsn,
                new_root_page,
                &new_root,
                self.page_size,
            )?;
            let meta = meta_bytes(self.meta_page, new_root_page, self.page_size);
            prev_lsn = write_raw(pool, wal, txn_id, prev_lsn, self.meta_page, meta)?;
        }
        wal.commit_mini_txn(txn_id, prev_lsn)?;
        Ok(())
    }

    /// Recursive insert. Returns `Some((separator_key, new_right_page))` when
    /// `pid` split and the caller must insert that separator one level up.
    #[allow(clippy::too_many_arguments)]
    fn insert_into(
        &self,
        pid: PageId,
        value: OrderedValue,
        rid: RowId,
        pool: &BufferPool,
        wal: &Wal,
        txn_id: u64,
        prev_lsn: &mut Lsn,
    ) -> Result<Option<(OrderedValue, PageId)>> {
        let page = pool.fetch_page(pid)?;
        let node = Node::deserialize(&page)?;
        pool.unpin(pid);
        match node {
            Node::Leaf { mut entries, next } => {
                let probe = (value.clone(), rowid_key(rid));
                let at = entries.binary_search_by(|(k, r)| (k.clone(), rowid_key(*r)).cmp(&probe));
                match at {
                    Ok(_) => return Ok(None), // exact (key,rid) duplicate — no-op
                    Err(pos) => entries.insert(pos, (value, rid)),
                }
                let leaf = Node::Leaf { entries, next };
                if leaf.body_len() <= body_capacity(self.page_size) {
                    *prev_lsn =
                        write_node(pool, wal, txn_id, *prev_lsn, pid, &leaf, self.page_size)?;
                    return Ok(None);
                }
                // Split the leaf in half; the right half moves to a new page and
                // becomes the current leaf's right sibling.
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
                Ok(Some((sep_key, right_page)))
            }
            Node::Internal { keys, children } => {
                let mut idx = keys.len();
                for (i, k) in keys.iter().enumerate() {
                    if &value < k {
                        idx = i;
                        break;
                    }
                }
                let child = children[idx];
                let split = self.insert_into(child, value, rid, pool, wal, txn_id, prev_lsn)?;
                let Some((sep_key, new_child)) = split else {
                    return Ok(None);
                };
                let mut keys = keys;
                let mut children = children;
                keys.insert(idx, sep_key);
                children.insert(idx + 1, new_child);
                let internal = Node::Internal { keys, children };
                if internal.body_len() <= body_capacity(self.page_size) {
                    *prev_lsn =
                        write_node(pool, wal, txn_id, *prev_lsn, pid, &internal, self.page_size)?;
                    return Ok(None);
                }
                // Split the internal node: the middle key rises to the parent.
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
                Ok(Some((up_key, right_page)))
            }
        }
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
            let page = pool.fetch_page(pid)?;
            let node = Node::deserialize(&page)?;
            pool.unpin(pid);
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
            pid = next;
        }
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
    Ok(lsn)
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
    fn insert_and_search_eq() {
        let mut e = env();
        let t = DiskBTree::create(&mut e.pool, &mut e.wal).unwrap();
        t.insert(OrderedValue::Int(5), rid(1, 0), &mut e.pool, &mut e.wal)
            .unwrap();
        t.insert(OrderedValue::Int(7), rid(2, 0), &mut e.pool, &mut e.wal)
            .unwrap();
        t.insert(OrderedValue::Int(5), rid(3, 0), &mut e.pool, &mut e.wal)
            .unwrap();
        let mut got = t.search_eq(&OrderedValue::Int(5), &mut e.pool).unwrap();
        got.sort_by_key(|r| r.page_id);
        assert_eq!(got, vec![rid(1, 0), rid(3, 0)]);
        assert!(t
            .search_eq(&OrderedValue::Int(99), &mut e.pool)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn range_queries() {
        let mut e = env();
        let t = DiskBTree::create(&mut e.pool, &mut e.wal).unwrap();
        for i in 1..=5 {
            t.insert(
                OrderedValue::Int(i),
                rid(i as u32, 0),
                &mut e.pool,
                &mut e.wal,
            )
            .unwrap();
        }
        assert_eq!(
            t.search_range(RangeOp::Lt, &OrderedValue::Int(3), &mut e.pool)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            t.search_range(RangeOp::Le, &OrderedValue::Int(3), &mut e.pool)
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            t.search_range(RangeOp::Gt, &OrderedValue::Int(3), &mut e.pool)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            t.search_range(RangeOp::Ge, &OrderedValue::Int(3), &mut e.pool)
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn search_dispatches_and_rejects_ne() {
        let mut e = env();
        let t = DiskBTree::create(&mut e.pool, &mut e.wal).unwrap();
        t.insert(OrderedValue::Int(1), rid(1, 0), &mut e.pool, &mut e.wal)
            .unwrap();
        assert!(t
            .search(CmpOp::Eq, &OrderedValue::Int(1), &mut e.pool)
            .unwrap()
            .is_some());
        assert!(t
            .search(CmpOp::Lt, &OrderedValue::Int(1), &mut e.pool)
            .unwrap()
            .is_some());
        assert!(t
            .search(CmpOp::Ne, &OrderedValue::Int(1), &mut e.pool)
            .unwrap()
            .is_none());
    }

    #[test]
    fn remove_drops_entry() {
        let mut e = env();
        let t = DiskBTree::create(&mut e.pool, &mut e.wal).unwrap();
        t.insert(OrderedValue::Int(1), rid(1, 0), &mut e.pool, &mut e.wal)
            .unwrap();
        t.insert(OrderedValue::Int(1), rid(2, 0), &mut e.pool, &mut e.wal)
            .unwrap();
        t.remove(&OrderedValue::Int(1), rid(1, 0), &mut e.pool, &mut e.wal)
            .unwrap();
        assert_eq!(
            t.search_eq(&OrderedValue::Int(1), &mut e.pool).unwrap(),
            vec![rid(2, 0)]
        );
    }

    #[test]
    fn text_keys() {
        let mut e = env();
        let t = DiskBTree::create(&mut e.pool, &mut e.wal).unwrap();
        t.insert(
            OrderedValue::Text("banana".into()),
            rid(1, 0),
            &mut e.pool,
            &mut e.wal,
        )
        .unwrap();
        t.insert(
            OrderedValue::Text("apple".into()),
            rid(2, 0),
            &mut e.pool,
            &mut e.wal,
        )
        .unwrap();
        assert_eq!(
            t.search_eq(&OrderedValue::Text("apple".into()), &mut e.pool)
                .unwrap(),
            vec![rid(2, 0)]
        );
    }

    /// Force many splits (a multi-level tree) and confirm every key is still
    /// findable — the core correctness proof for node splitting + routing.
    #[test]
    fn many_inserts_force_splits_and_stay_searchable() {
        let mut e = env();
        let t = DiskBTree::create(&mut e.pool, &mut e.wal).unwrap();
        // Enough to force several leaf splits and at least one internal level
        // (~480 entries/leaf), proving split + separator routing. Kept modest
        // because each insert is its own fsyncing mini-txn.
        let n = 2000i64;
        for i in 0..n {
            t.insert(
                OrderedValue::Int(i),
                rid(i as u32, 0),
                &mut e.pool,
                &mut e.wal,
            )
            .unwrap();
        }
        for i in 0..n {
            let got = t.search_eq(&OrderedValue::Int(i), &mut e.pool).unwrap();
            assert_eq!(got, vec![rid(i as u32, 0)], "key {i} missing after splits");
        }
        // Range over the whole set.
        assert_eq!(
            t.search_range(RangeOp::Ge, &OrderedValue::Int(0), &mut e.pool)
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
        let mut e = env();
        let t = DiskBTree::create(&mut e.pool, &mut e.wal).unwrap();
        // Neighbours on both sides so the hot key's run sits between other keys
        // and its leaves split with real separators around it.
        let dup = 3000u32; // far more than one leaf holds (~480 int entries)
        for i in 0..500 {
            t.insert(OrderedValue::Int(1), rid(i, 0), &mut e.pool, &mut e.wal)
                .unwrap();
        }
        for i in 0..dup {
            t.insert(OrderedValue::Int(2), rid(i, 1), &mut e.pool, &mut e.wal)
                .unwrap();
        }
        for i in 0..500 {
            t.insert(OrderedValue::Int(3), rid(i, 2), &mut e.pool, &mut e.wal)
                .unwrap();
        }
        let got = t.search_eq(&OrderedValue::Int(2), &mut e.pool).unwrap();
        assert_eq!(got.len(), dup as usize, "must return every duplicate");
        assert!(got.iter().all(|r| r.slot == 1));
        // Neighbours unaffected.
        assert_eq!(
            t.search_eq(&OrderedValue::Int(1), &mut e.pool)
                .unwrap()
                .len(),
            500
        );
        assert_eq!(
            t.search_eq(&OrderedValue::Int(3), &mut e.pool)
                .unwrap()
                .len(),
            500
        );
        // Remove one from deep in the run, then re-count.
        t.remove(&OrderedValue::Int(2), rid(1500, 1), &mut e.pool, &mut e.wal)
            .unwrap();
        assert_eq!(
            t.search_eq(&OrderedValue::Int(2), &mut e.pool)
                .unwrap()
                .len(),
            dup as usize - 1
        );
    }

    /// The tree survives being reconstructed from just its meta page id — the
    /// Phase-3 "no rebuild on open" property, at the module level.
    #[test]
    fn reopen_from_meta_page_only() {
        let mut e = env();
        let meta = {
            let t = DiskBTree::create(&mut e.pool, &mut e.wal).unwrap();
            for i in 0..500i64 {
                t.insert(
                    OrderedValue::Int(i),
                    rid(i as u32, 0),
                    &mut e.pool,
                    &mut e.wal,
                )
                .unwrap();
            }
            t.meta_page()
        };
        // A brand-new handle over the same meta page — no rebuild, no scan.
        let t2 = DiskBTree::new(meta, DEFAULT_PAGE_SIZE as usize);
        assert_eq!(
            t2.search_eq(&OrderedValue::Int(250), &mut e.pool).unwrap(),
            vec![rid(250, 0)]
        );
    }
}
