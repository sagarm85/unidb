//! Auth principal seam (coordination plumbing, inert in this milestone).
//!
//! [`AuthPrincipal`] is the carrier for "everything the HTTP auth layer
//! learned about the caller": the `sub` claim (already used for RLS /
//! privilege checks via `Option<&str>` elsewhere), plus the full flattened
//! JWT claim set and a role list. Today nothing *consumes* `claims`/`roles` —
//! they are threaded down to [`crate::sql::executor::ExecCtx`] so later work
//! (policy predicates that read arbitrary claims, role-based grants) can land
//! without another plumbing pass. See `execute_sql_as_principal` in `lib.rs`
//! for the one wired call path.

use std::collections::BTreeMap;

/// Everything known about the authenticated caller for one request/statement.
///
/// `subject` is exactly what `CurrentUser`/`execute_sql_as`'s `user: Option<&str>`
/// carried before this seam existed — it remains the sole input to RLS /
/// privilege checks for now. `claims` and `roles` are carried alongside but not
/// yet consumed by any policy logic.
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
