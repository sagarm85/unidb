//! Auth principal seam (coordination plumbing, inert in this milestone).
//!
//! [`AuthPrincipal`] is the carrier for "everything the HTTP auth layer
//! learned about the caller": the `sub` claim (already used for RLS /
//! privilege checks via `Option<&str>` elsewhere), plus the full flattened
//! JWT claim set and a role list. `claims` now backs `auth.uid()`/`auth.jwt()
//! ->> '...'` RLS-policy substitution (item 122, Workstream B1/B2) — see
//! `crate::sql::logical::substitute_auth_context_in_plan`/`_in_expr`. `roles`
//! is still unconsumed (parked for role-scoped policies, B3/B4). Both fields
//! are threaded down to [`crate::sql::executor::ExecCtx`]. See
//! `execute_sql_as_principal` in `lib.rs` for the one wired writer-path call
//! site, and `crate::read_handle::ReadHandle::execute_sql_as_principal` for
//! the concurrent-read-path equivalent.

use std::collections::BTreeMap;

/// Everything known about the authenticated caller for one request/statement.
///
/// `subject` is exactly what `CurrentUser`/`execute_sql_as`'s `user: Option<&str>`
/// carried before this seam existed — it remains the sole input to
/// `current_user()`/RLS privilege checks. `claims` backs `auth.jwt() ->>
/// '...'` policy substitution (item 122, B2); `roles` is still carried
/// alongside but not yet consumed by any policy logic (parked for B3/B4).
#[derive(Clone, Debug, Default)]
pub struct AuthPrincipal {
    /// The `sub` claim / unidb username. `None` = the implicit superuser
    /// (embedded API / anonymous-but-authenticated client).
    pub subject: Option<String>,
    /// The full flattened JWT claim set (minus `sub`, which lives in
    /// `subject`). Empty when the principal wasn't built from a token.
    pub claims: BTreeMap<String, serde_json::Value>,
    /// Roles associated with this principal. Empty for now — no role
    /// resolution logic exists yet; this is a placeholder for later work.
    pub roles: Vec<String>,
}

impl AuthPrincipal {
    /// Build a principal from just a subject, with empty claims/roles — the
    /// exact shape every existing `execute_sql_as(user, ..)` caller implies.
    pub fn user(subject: Option<String>) -> Self {
        Self {
            subject,
            claims: BTreeMap::new(),
            roles: Vec::new(),
        }
    }
}
