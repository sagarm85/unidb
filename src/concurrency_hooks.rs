// The on_read()/on_write() seam (D11).
//
// Both are intentionally no-ops in M1. Their purpose is purely structural:
// by threading a call through every scan, point-lookup, and mutation path
// now, a future SERIALIZABLE/SSI implementation can turn these into
// read-set/write-set tracking for rw-antidependency detection *without*
// rewriting the executor to find those call sites — this is the "retrofit
// trap" D11 explicitly calls out avoiding. Do not add logic here until an
// SSI milestone actually needs it.

use crate::{format::Xid, heap::RowId};

/// Called after a tuple has passed visibility filtering and is about to be
/// returned to the caller, from every `Heap::get`/`Heap::scan` call site.
pub fn on_read(_xid: Xid, _row: RowId) {
    // intentionally empty — D11 seam
}

/// Called immediately before a write (insert/update/delete) is applied to a
/// row, from every `Heap::insert`/`Heap::update`/`Heap::delete` call site.
pub fn on_write(_xid: Xid, _row: RowId) {
    // intentionally empty — D11 seam
}
