//! Auth principal seam (coordination plumbing, inert in this milestone).
//!
//! [`AuthPrincipal`] is the carrier for "everything the HTTP auth layer
//! learned about the caller": the `sub` claim (already used for RLS /
//! privilege checks via `Option<&str>` elsewhere), plus the full flattened
//! JWT claim set and a role list. `claims` backs `auth.uid()`/`auth.jwt()
//! ->> '...'` RLS-policy substitution (item 122, Workstream B1/B2) — see
//! `crate::sql::logical::substitute_auth_context_in_plan`/`_in_expr`.
//!
//! `roles` is populated **engine-side only** (item 122, B3): the HTTP auth
//! layer (`crate::server::auth::require_jwt`) never sets it — it has no
//! `authz`/`RoleStore` access to resolve it correctly, and role resolution is
//! deliberately not the token-verification layer's job. `Engine::
//! execute_sql_inner_as_principal` (writer path) and `ReadHandle::
//! execute_sql_inner` (concurrent read path) instead call
//! `crate::authz::RoleStore::effective_roles(principal.subject.as_deref(),
//! &principal.claims)` themselves and build a fresh `AuthPrincipal` carrying
//! the *resolved* roles for every downstream call — so by the time
//! `ExecCtx::auth_roles` is populated, it holds the caller's real effective
//! roles (`anon`/`authenticated`/`service_role`/granted roles), not whatever
//! (always empty) value the original principal came in with. Both `claims`
//! and `roles` are threaded down to [`crate::sql::executor::ExecCtx`]. See
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
    /// Effective roles for this principal (item 122, B3). Always empty on a
    /// principal built by the HTTP auth layer (`require_jwt`) or by
    /// [`AuthPrincipal::user`] — populated only by the engine's own
    /// `effective_roles`-resolved copy used internally for execution (see
    /// this module's doc comment). Do not populate this field from
    /// caller-supplied data; a widened role list here would widen RLS access.
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
