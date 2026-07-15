# FK constraint error message uses wrong direction on parent DELETE

**Type:** Bug (UX / error messaging)
**Status:** OPEN

## Problem

When a user tries to DELETE a parent row that is still referenced by child rows,
the engine returns a FK violation message phrased as a child-insert error:

```
FOREIGN KEY constraint violated on table 'orders':
column 'customer_id' value 2 has no matching row in 'customers'
```

This message reads as if an INSERT/UPDATE on `orders` tried to set
`customer_id = 2` and no such customer exists — the exact opposite of what
actually happened.  The user deleted (or attempted to delete) customer 2, and
the engine correctly blocked it, but the message is from the wrong direction.

## Desired behaviour

On a DELETE of a parent row that is still referenced by child rows, the error
message should clearly identify:

1. The parent table and the row being deleted
2. The child table and column that still holds a reference
3. The count of blocking child rows (optional but helpful)

Example:

```
FOREIGN KEY constraint violated: cannot delete from 'customers' —
'orders'.customer_id still references id = 2 (N row(s) found)
```

The current message format is correct for the child-INSERT / child-UPDATE case
and should stay as-is for that path.

## Impact

- Users see a confusing, backward error when deleting any parent row that has
  dependent child rows (customers with orders, orders with order_items, etc.).
- Particularly visible in the Studio Record Browser where the error banner is
  the user's only feedback after a failed delete.

## Acceptance criteria

- [ ] DELETE on a parent row with existing child references returns an error
      message that names the parent table + row key and the blocking child table
      + column.
- [ ] INSERT / UPDATE on a child row with a missing parent continues to use the
      existing message format ("value X has no matching row in 'parent'").
- [ ] No change to HTTP status code (400 is correct).
