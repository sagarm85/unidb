//! Concurrent read handle (6b).
//!
//! A `Send + Sync + Clone` view for reads that run **off** the single writer
//! thread. It holds only shared state — the page-file mmap behind an
//! `Arc<RwLock<..>>` ([`SharedPageReader`]) and the transaction snapshot state
//! ([`SharedTxn`]) — so many readers execute in parallel with each other and
//! with the writer, coordinating solely through MVCC snapshots rather than the
//! writer's request channel.
//!
//! A read here allocates no xid and writes no WAL: [`txn::read_snapshot`] gives
//! a self-contained READ COMMITTED snapshot plus a sentinel `self_xid`, and the
//! bytes come straight from the shared mmap under its read-lock. This is the
//! payoff of the earlier interior-mutability groundwork: under MVCC a reader
//! genuinely needs nothing exclusive.
//!
//! v1 covers point reads (`get`). Concurrent SQL `SELECT` is the same pattern
//! layered on a shared catalog + a read-only executor path (tracked in
//! `docs/backlog/group_commit_and_read_concurrency.md`).

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::auth_principal::AuthPrincipal;
use crate::authz::RoleStore;
use crate::bufferpool::{PageReader, SharedPageReader};
use crate::catalog::Catalog;
use crate::error::{DbError, Result};
use crate::heap::RowId;
use crate::mvcc::is_visible;
use crate::sql::executor::{exec_select_readonly, plan_is_concurrent_read, ExecResult};
use crate::sql::logical::{
    apply_rls_skip_current_user, apply_rls_with_auth, substitute_auth_context_in_plan,
    substitute_current_user_in_plan, LogicalPlan,
};
use crate::sql::parser::parse_sql;
use crate::txn::{self, SharedTxn};

/// Whether every statement in `sql` can run on the concurrent read path (6b):
/// a non-empty batch of plain `SELECT`s with no `NEAR`. Lets a caller (the
/// server's `POST /sql`) route reads to [`ReadHandle::execute_sql`] and
/// everything else (writes, DDL, NEAR) to the single writer thread. Parse
/// failures return `false` so the writer path can surface the real error.
pub fn is_concurrent_read_sql(sql: &str) -> bool {
    match parse_sql(sql) {
        Ok(plans) => !plans.is_empty() && plans.iter().all(plan_is_concurrent_read),
        Err(_) => false,
    }
}

/// A cloneable, thread-safe handle for concurrent reads (6b). Obtain one from
/// [`crate::Engine::read_handle`].
#[derive(Clone)]
pub struct ReadHandle {
    reader: SharedPageReader,
    txn: SharedTxn,
    catalog: Arc<RwLock<Catalog>>,
    /// Role store for RLS bypass decisions (item 103): superusers and the
    /// no-`sub` (embedded) path must bypass `current_user`-referencing
    /// policies; also resolves effective roles (item 122, B3) for the
    /// role-scoped policy gate and the `service_role` bypass.
    authz: Arc<RoleStore>,
    /// Audit trail (item-103 lesson, item 122 B3): a `service_role` RLS
    /// bypass on this read path is logged distinctly from the unaudited
    /// implicit-superuser bypass, exactly like the writer path
    /// (`Engine::execute_sql_inner_as_principal`).
    audit: Arc<crate::audit::AuditLog>,
}

impl ReadHandle {
    pub(crate) fn new(
        reader: SharedPageReader,
        txn: SharedTxn,
        catalog: Arc<RwLock<Catalog>>,
        authz: Arc<RoleStore>,
        audit: Arc<crate::audit::AuditLog>,
    ) -> Self {
        Self {
            reader,
            txn,
            catalog,
            authz,
            audit,
        }
    }

    /// Execute one or more **read-only** SQL statements on the concurrent read
    /// path (6b) as the embedded/no-user superuser. Policies that reference
    /// `current_user` are skipped (item 103); literal-value policies apply.
    /// For user-aware RLS use [`ReadHandle::execute_sql_as`].
    pub fn execute_sql(&self, sql: &str) -> Result<Vec<ExecResult>> {
        self.execute_sql_as(None, sql)
    }

    /// Execute one or more **read-only** SQL statements with user-aware RLS
    /// (item 103). `user = None` is the embedded/no-sub superuser path and
    /// bypasses all `current_user`-containing policies. A named superuser
    /// likewise bypasses them. A regular named user has `current_user`
    /// substituted and all applicable policies AND-rewritten into the plan.
    ///
    /// Carries no verified claims — `auth.jwt()`-referencing policies fail
    /// closed here exactly as they would for an authenticated caller with no
    /// matching claim. Use [`ReadHandle::execute_sql_as_principal`] to carry
    /// claims through (item 122, B1/B2).
    pub fn execute_sql_as(&self, user: Option<&str>, sql: &str) -> Result<Vec<ExecResult>> {
        self.execute_sql_inner(user, &BTreeMap::new(), sql)
    }

    /// Like [`ReadHandle::execute_sql_as`] but threads the full
    /// [`AuthPrincipal`] through — so `auth.uid()`/`auth.jwt() ->> '...'`
    /// policies (item 122, B1/B2) resolve correctly on the concurrent read
    /// path too, not just the writer path. `principal.subject` plays the same
    /// role as `execute_sql_as`'s `user` argument.
    pub fn execute_sql_as_principal(
        &self,
        principal: &AuthPrincipal,
        sql: &str,
    ) -> Result<Vec<ExecResult>> {
        self.execute_sql_inner(principal.subject.as_deref(), &principal.claims, sql)
    }

    fn execute_sql_inner(
        &self,
        user: Option<&str>,
        claims: &BTreeMap<String, serde_json::Value>,
        sql: &str,
    ) -> Result<Vec<ExecResult>> {
        // Determine if this caller should bypass current_user-referencing policies.
        // None == embedded/no-sub == implicit superuser; a named SUPERUSER user
        // also bypasses. In open/bootstrap mode (no users registered) every caller
        // is an effective superuser.
        //
        // item 122, B3: also resolve the caller's effective roles (engine-side,
        // via `self.authz`) — a `service_role` claim bypasses RLS the same way a
        // superuser does, but stays on the audited path (item-103 lesson).
        let effective_roles = self.authz.effective_roles(user, claims);
        let is_service_role = effective_roles
            .iter()
            .any(|r| r == crate::authz::SERVICE_ROLE);
        let skip_current_user_policies = match user {
            None => true,
            Some(u) => self.authz.is_superuser(u) || !self.authz.has_users(),
        } || is_service_role;
        let plans = parse_sql(sql)?;
        let mut out = Vec::with_capacity(plans.len());
        for mut plan in plans {
            // Substitute current_user() in the user-supplied SQL predicates
            // first, then apply RLS (which may inject more CurrentUser nodes),
            // then substitute again for the injected policy expressions.
            if let Some(u) = user {
                substitute_current_user_in_plan(&mut plan, u);
            }
            // Item 122 (B1/B2): substitute auth.uid()/auth.jwt() too, same
            // unconditional-call/fail-closed contract as the writer path
            // (`execute_sql_inner_as_principal` in lib.rs).
            substitute_auth_context_in_plan(&mut plan, user, claims);
            let mut plan = if skip_current_user_policies {
                // Superuser / no-sub / service_role: skip any policy that
                // references current_user (or, item 122, auth.uid()/
                // auth.jwt()). Literal-value policies still apply, matching
                // the behaviour of execute_sql_inner on the writer path.
                if is_service_role {
                    self.audit
                        .record_admin(user, None, "service_role_rls_bypass", "", true);
                }
                apply_rls_skip_current_user(plan, &self.catalog_read())
            } else {
                // Regular named user: apply all applicable policies, role-gated
                // (item 122, B4) by the caller's effective roles.
                apply_rls_with_auth(plan, &self.catalog_read(), user, claims, &effective_roles)
            };
            // Second substitution resolves CurrentUser/AuthUid/AuthClaim nodes
            // injected by the RLS policy expressions (only matters for the
            // non-skip path).
            if let Some(u) = user {
                substitute_current_user_in_plan(&mut plan, u);
            }
            substitute_auth_context_in_plan(&mut plan, user, claims);
            if !plan_is_concurrent_read(&plan) {
                return Err(DbError::SqlPlan(
                    "read handle executes only plain read-only SELECT (no writes, DDL, or NEAR)"
                        .into(),
                ));
            }
            let LogicalPlan::Select {
                table,
                projection,
                predicate,
            } = &plan
            else {
                unreachable!("plan_is_concurrent_read guarantees a Select");
            };
            // The `_reg` guard holds the vacuum horizon back (M10.a) for the
            // whole scan, so a concurrent writer-thread vacuum can't reclaim a
            // version this read still needs. Dropped at the end of each
            // statement's scan.
            let (snapshot, self_xid, _reg) = txn::read_snapshot(&self.txn);
            out.push(exec_select_readonly(
                table,
                projection,
                predicate,
                &self.catalog_read(),
                &snapshot,
                self_xid,
                &self.reader,
            )?);
        }
        Ok(out)
    }

    fn catalog_read(&self) -> std::sync::RwLockReadGuard<'_, Catalog> {
        self.catalog.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Read one untyped row by [`RowId`] under a fresh READ COMMITTED snapshot,
    /// with no writer-thread involvement. Returns
    /// [`DbError::NoVisibleVersion`] if the tuple version at `row_id` is not
    /// visible to that snapshot (superseded, deleted, or never committed) —
    /// exactly the same visibility rule the writer-side `Engine::get` applies.
    pub fn get(&self, row_id: RowId) -> Result<Vec<u8>> {
        // `_reg` holds the vacuum horizon back (M10.a) until this read returns.
        let (snapshot, self_xid, _reg) = txn::read_snapshot(&self.txn);
        let page = self.reader.read_page(row_id.page_id)?;
        let th = page.tuple_header(row_id.slot)?;
        if is_visible(th.xmin, th.xmax, &snapshot, self_xid) {
            Ok(page.get(row_id.slot)?.to_vec())
        } else {
            Err(DbError::NoVisibleVersion {
                page_id: row_id.page_id,
                slot: row_id.slot,
            })
        }
    }
}

/// Compile-time proof that a `ReadHandle` can be shared across threads — the
/// whole point of 6b. (`Engine` itself stays deliberately non-`Sync`.)
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ReadHandle>();
};
