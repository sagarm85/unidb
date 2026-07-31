// Users, roles, and privileges (P6.e / item-24 Z1).
//
// A persisted authorization store: users (optionally superuser), roles, role
// membership (users→roles and roles→roles, resolved transitively), and
// per-table privileges (SELECT/INSERT/UPDATE/DELETE, or ALL). It is the identity
// + access-control layer that turns the single-shared-JWT server into per-user
// auth, and gives the embedded API a `GRANT`/`REVOKE` surface.
//
// **Superuser model:** the embedded API runs as an implicit superuser (identity
// `None`) — every existing embedded call is unchanged and unrestricted. A named
// user created `SUPERUSER` can also administer. Auth DDL (CREATE/DROP USER|ROLE,
// GRANT, REVOKE, CREATE/DROP POLICY) and schema DDL require superuser in v1;
// data statements (SELECT/INSERT/UPDATE/DELETE) require the matching privilege
// on the table.
//
// The auth DDL grammar is small and parsed here (not via `sqlparser`, whose
// GRANT/ROLE AST is awkward) so the surface stays controlled:
//
//   CREATE USER <name> [SUPERUSER]        DROP USER <name>
//   CREATE ROLE <name>                    DROP ROLE <name>
//   GRANT <priv,.. | ALL> ON <table> TO <grantee>
//   GRANT <role> TO <grantee>                       (role membership)
//   REVOKE <priv,.. | ALL> ON <table> FROM <grantee>
//   REVOKE <role> FROM <grantee>
//   CREATE POLICY <name> ON <table> FOR <op> USING (<predicate>) [WITH CHECK (<expr>)]
//   DROP POLICY <name> ON <table>
//
// Roles/users/grants persist to `roles.json` (control-plane metadata, so
// `serde` is fine per CLAUDE.md §4). Named policies persist in the catalog
// blob (alongside `rls_policy`) so there is no FORMAT_VERSION bump.
// `Send + Sync` for the shared `Engine`.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use argon2::{
    password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use serde::{Deserialize, Serialize};

use crate::error::{DbError, Result};

/// Hash `password` with argon2id (library defaults: m=19 MiB, t=2, p=1),
/// returning the self-describing PHC string (algorithm + params + salt +
/// hash) that [`verify_password_hash`] can verify against later. Never
/// returns or logs the plaintext.
fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| DbError::Authz(format!("password hash: {e}")))
}

/// Constant-shape verify: parses the PHC `hash` string and checks `password`
/// against it. Returns `false` (never an error) on a malformed stored hash —
/// recovery code must not panic on bad control-plane metadata, and a corrupt
/// hash should simply fail closed.
fn verify_password_hash(hash: &str, password: &str) -> bool {
    match PasswordHash::new(hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A fixed, process-lifetime-stable argon2id hash used to run a same-cost
/// "verify" when the target user doesn't exist or has no stored credential,
/// so `RoleStore::verify_password` does the same amount of work (and takes
/// the same code path) regardless of *why* login should fail — no
/// user-enumeration timing/shape oracle. Computed once per process (not a
/// hash of any real credential) and cached; the exact bytes are irrelevant,
/// only that a real argon2id verify runs against them.
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY
        .get_or_init(|| {
            hash_password("unidb-dummy-password-for-timing-parity").unwrap_or_else(|_| {
                // Fallback fixed PHC string in the vanishingly unlikely case
                // hashing itself fails (e.g. RNG unavailable) — still a valid
                // argon2id hash shape, just not freshly salted.
                "$argon2id$v=19$m=19456,t=2,p=1$AAAAAAAAAAAAAAAAAAAAAA$\
                 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                    .to_string()
            })
        })
        .as_str()
}

/// A table-level privilege.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
}

impl Privilege {
    fn parse(s: &str) -> Option<Privilege> {
        match s.to_ascii_uppercase().as_str() {
            "SELECT" => Some(Privilege::Select),
            "INSERT" => Some(Privilege::Insert),
            "UPDATE" => Some(Privilege::Update),
            "DELETE" => Some(Privilege::Delete),
            _ => None,
        }
    }
    /// The four grantable privileges (what `ALL` expands to).
    fn all() -> [Privilege; 4] {
        [
            Privilege::Select,
            Privilege::Insert,
            Privilege::Update,
            Privilege::Delete,
        ]
    }
    /// String representation for catalog exposure (Z5).
    pub fn as_str(self) -> &'static str {
        match self {
            Privilege::Select => "SELECT",
            Privilege::Insert => "INSERT",
            Privilege::Update => "UPDATE",
            Privilege::Delete => "DELETE",
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct AuthState {
    /// username → superuser?
    users: BTreeMap<String, bool>,
    roles: BTreeSet<String>,
    /// grantee (user or role) → roles it is a member of.
    memberships: BTreeMap<String, BTreeSet<String>>,
    /// grantee → table → privileges.
    table_grants: BTreeMap<String, BTreeMap<String, BTreeSet<Privilege>>>,
    /// username → argon2id PHC hash string (item 121, A1). Never the
    /// plaintext. `#[serde(default)]` so a pre-A1 `roles.json` (no
    /// `credentials` key) deserializes with an empty map — no
    /// FORMAT_VERSION bump. Persisted (it must be, to survive a restart);
    /// deliberately kept out of `Debug` (see the manual impl below) so it
    /// never leaks into `tracing::debug!`/`{:?}` logging.
    #[serde(default)]
    credentials: BTreeMap<String, String>,
}

/// Manual `Debug`: every field except `credentials`, which is redacted to a
/// count. Prevents an argon2id hash (a secret the CLAUDE.md rules for this
/// milestone say must never appear in logs) from leaking via `{:?}` if
/// `AuthState`/`RoleStore` internals are ever debug-printed.
impl std::fmt::Debug for AuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthState")
            .field("users", &self.users)
            .field("roles", &self.roles)
            .field("memberships", &self.memberships)
            .field("table_grants", &self.table_grants)
            .field(
                "credentials",
                &format!("<{} redacted>", self.credentials.len()),
            )
            .finish()
    }
}

/// Operation scope for a named RLS policy (item-24 Z1).
/// `All` expands to "applies to every DML operation".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyOp {
    Select,
    Insert,
    Update,
    Delete,
    All,
}

impl PolicyOp {
    fn parse(s: &str) -> Option<PolicyOp> {
        match s.to_ascii_uppercase().as_str() {
            "SELECT" => Some(PolicyOp::Select),
            "INSERT" => Some(PolicyOp::Insert),
            "UPDATE" => Some(PolicyOp::Update),
            "DELETE" => Some(PolicyOp::Delete),
            "ALL" => Some(PolicyOp::All),
            _ => None,
        }
    }

    /// String representation for catalog exposure (Z5).
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyOp::Select => "SELECT",
            PolicyOp::Insert => "INSERT",
            PolicyOp::Update => "UPDATE",
            PolicyOp::Delete => "DELETE",
            PolicyOp::All => "ALL",
        }
    }
}

/// A named RLS policy stored in the catalog `TableDef.policies` (item-24 Z1).
/// Mirrors Postgres `pg_policy`: a name, an operation scope, a USING predicate
/// (row-filter — which rows may be seen/affected), and an optional WITH CHECK
/// predicate (write-side — new row must satisfy this; defaults to `using_expr`
/// when absent, per Postgres semantics).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolicyDef {
    pub name: String,
    pub table: String,
    pub op: PolicyOp,
    /// Raw SQL predicate (the `USING (…)` clause), stored verbatim for
    /// catalog exposure.  Re-parsed into `Expr` at apply time.
    pub using_expr: String,
    /// Optional `WITH CHECK (…)` predicate (item-24 R-a).  When `None` the
    /// USING expression doubles as the write-side check, per Postgres
    /// semantics.  `#[serde(default)]` ensures pre-R-a catalog blobs
    /// deserialize with `None` — no FORMAT_VERSION bump required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub with_check_sql: Option<String>,
}

/// A parsed auth-DDL statement.
#[derive(PartialEq)]
pub enum AuthStmt {
    CreateUser {
        name: String,
        superuser: bool,
        /// Plaintext from the optional `PASSWORD '<pw>'` clause (item 121,
        /// A1) — hashed with argon2id before it ever reaches `AuthState`.
        /// Never stored or logged as-is; kept out of `Debug` (see the manual
        /// impl below) so a `{:?}` of a pending statement can't leak it
        /// either.
        password: Option<String>,
    },
    DropUser(String),
    CreateRole(String),
    DropRole(String),
    GrantPrivs {
        privs: Vec<Privilege>,
        table: String,
        grantee: String,
    },
    RevokePrivs {
        privs: Vec<Privilege>,
        table: String,
        grantee: String,
    },
    GrantRole {
        role: String,
        grantee: String,
    },
    RevokeRole {
        role: String,
        grantee: String,
    },
    /// `CREATE POLICY <name> ON <table> FOR <op> USING (<predicate>) [WITH CHECK (<expr>)]` (Z1/R-a).
    CreatePolicy(PolicyDef),
    /// `DROP POLICY <name> ON <table>` (Z1).
    DropPolicy {
        name: String,
        table: String,
    },
}

/// Manual `Debug`: redacts `CreateUser`'s `password` field to `Some("<redacted>")`/
/// `None` instead of the plaintext, so `tracing::info!(?stmt, ..)` in
/// [`RoleStore::apply`] (and any other `{:?}`/audit-log use of a pending
/// statement) can never print a password.
impl std::fmt::Debug for AuthStmt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthStmt::CreateUser {
                name,
                superuser,
                password,
            } => f
                .debug_struct("CreateUser")
                .field("name", name)
                .field("superuser", superuser)
                .field("password", &password.as_ref().map(|_| "<redacted>"))
                .finish(),
            AuthStmt::DropUser(name) => f.debug_tuple("DropUser").field(name).finish(),
            AuthStmt::CreateRole(name) => f.debug_tuple("CreateRole").field(name).finish(),
            AuthStmt::DropRole(name) => f.debug_tuple("DropRole").field(name).finish(),
            AuthStmt::GrantPrivs {
                privs,
                table,
                grantee,
            } => f
                .debug_struct("GrantPrivs")
                .field("privs", privs)
                .field("table", table)
                .field("grantee", grantee)
                .finish(),
            AuthStmt::RevokePrivs {
                privs,
                table,
                grantee,
            } => f
                .debug_struct("RevokePrivs")
                .field("privs", privs)
                .field("table", table)
                .field("grantee", grantee)
                .finish(),
            AuthStmt::GrantRole { role, grantee } => f
                .debug_struct("GrantRole")
                .field("role", role)
                .field("grantee", grantee)
                .finish(),
            AuthStmt::RevokeRole { role, grantee } => f
                .debug_struct("RevokeRole")
                .field("role", role)
                .field("grantee", grantee)
                .finish(),
            AuthStmt::CreatePolicy(policy) => f.debug_tuple("CreatePolicy").field(policy).finish(),
            AuthStmt::DropPolicy { name, table } => f
                .debug_struct("DropPolicy")
                .field("name", name)
                .field("table", table)
                .finish(),
        }
    }
}

/// Persisted authorization store.
pub struct RoleStore {
    path: PathBuf,
    inner: Mutex<AuthState>,
}

impl RoleStore {
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join("roles.json");
        let inner = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "roles.json unreadable — starting with no roles");
                AuthState::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => AuthState::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self {
            path,
            inner: Mutex::new(inner),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AuthState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn persist(&self, st: &AuthState) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(st)
            .map_err(|e| DbError::Authz(format!("serialize roles: {e}")))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Whether `user` is a superuser (a named user created `SUPERUSER`).
    pub fn is_superuser(&self, user: &str) -> bool {
        self.lock().users.get(user).copied().unwrap_or(false)
    }

    /// Whether any user is registered. When empty, the engine runs in **open /
    /// bootstrap mode** (everyone is an effective superuser) — this preserves
    /// the pre-P6.e "any valid token grants full access" behavior until an
    /// operator creates the first user. Create a `SUPERUSER` first to bootstrap.
    pub fn has_users(&self) -> bool {
        !self.lock().users.is_empty()
    }

    /// Set (or replace) `user`'s password credential, hashed with argon2id
    /// (item 121, A1). Errors if `user` doesn't exist. This is the Rust-API
    /// path (`Engine::set_password`); `CREATE USER ... PASSWORD '<pw>'`
    /// hashes inline in [`Self::apply`] instead.
    pub fn set_password(&self, user: &str, password: &str) -> Result<()> {
        let hash = hash_password(password)?;
        let mut st = self.lock();
        if !st.users.contains_key(user) {
            return Err(DbError::Authz(format!("user '{user}' not found")));
        }
        st.credentials.insert(user.to_string(), hash);
        self.persist(&st)?;
        tracing::info!(user = %user, "password credential set");
        Ok(())
    }

    /// Verify `password` against `user`'s stored credential (item 121, A2).
    ///
    /// Returns `false` uniformly for every failure case — unknown user, user
    /// with no stored credential, or a genuine mismatch — and always runs a
    /// real argon2id verify (against [`dummy_hash`] in the first two cases)
    /// so the cost and code path are identical regardless of *why* it fails.
    /// This is the login authentication check: no user-enumeration
    /// timing/shape oracle.
    pub fn verify_password(&self, user: &str, password: &str) -> bool {
        let hash = self.lock().credentials.get(user).cloned();
        match hash {
            Some(h) => verify_password_hash(&h, password),
            None => {
                // Constant-shape dummy verify: burns the same argon2id cost,
                // discards the result, and always reports failure.
                let _ = verify_password_hash(dummy_hash(), password);
                false
            }
        }
    }

    /// Whether `user` holds `priv` on `table`, resolving role membership
    /// transitively. Superusers hold every privilege.
    pub fn has_privilege(&self, user: &str, table: &str, priv_: Privilege) -> bool {
        let st = self.lock();
        if st.users.get(user).copied().unwrap_or(false) {
            return true;
        }
        // Collect the user + every role reachable through membership.
        let mut grantees: HashSet<String> = HashSet::new();
        let mut stack = vec![user.to_string()];
        while let Some(g) = stack.pop() {
            if !grantees.insert(g.clone()) {
                continue;
            }
            if let Some(roles) = st.memberships.get(&g) {
                for r in roles {
                    stack.push(r.clone());
                }
            }
        }
        grantees.iter().any(|g| {
            st.table_grants
                .get(g)
                .and_then(|t| t.get(table))
                .map(|p| p.contains(&priv_))
                .unwrap_or(false)
        })
    }

    /// Snapshot of the users (name, superuser).
    pub fn users(&self) -> Vec<(String, bool)> {
        self.lock()
            .users
            .iter()
            .map(|(n, s)| (n.clone(), *s))
            .collect()
    }

    /// Snapshot of all role names (item-24 Z5: `unidb_catalog.roles`).
    pub fn roles(&self) -> Vec<String> {
        self.lock().roles.iter().cloned().collect()
    }

    /// Snapshot of all role memberships as `(role, member)` pairs.
    /// `member` is a user or role that has been granted membership in `role`.
    /// (item-24 Z4: `unidb_catalog.role_members`)
    pub fn memberships(&self) -> Vec<(String, String)> {
        let st = self.lock();
        let mut out = Vec::new();
        for (member, roles) in &st.memberships {
            for role in roles {
                out.push((role.clone(), member.clone()));
            }
        }
        out
    }

    /// Snapshot of all grants as `(grantee, table, privilege)` triples
    /// (item-24 Z5: `unidb_catalog.grants`).
    pub fn grants(&self) -> Vec<(String, String, Privilege)> {
        let st = self.lock();
        let mut out = Vec::new();
        for (grantee, tables) in &st.table_grants {
            for (table, privs) in tables {
                for p in privs {
                    out.push((grantee.clone(), table.clone(), *p));
                }
            }
        }
        out
    }

    /// Table-level grants for a user, collected as `(table, [privilege_names])`.
    /// Used by `GET /auth/whoami` (item 100).
    pub fn table_grants_for(&self, user: &str) -> Vec<(String, Vec<String>)> {
        let st = self.lock();
        match st.table_grants.get(user) {
            Some(tables) => tables
                .iter()
                .map(|(tbl, privs)| {
                    (
                        tbl.clone(),
                        privs.iter().map(|p| p.as_str().to_string()).collect(),
                    )
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Roles a user belongs to (direct memberships only; not transitive).
    /// Used by `GET /auth/whoami` (item 100).
    pub fn roles_for(&self, user: &str) -> Vec<String> {
        let st = self.lock();
        st.memberships
            .get(user)
            .map(|roles| roles.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Apply a parsed auth-DDL statement. The caller must have already checked
    /// the actor is a superuser.
    pub fn apply(&self, stmt: &AuthStmt) -> Result<()> {
        let mut st = self.lock();
        match stmt {
            AuthStmt::CreateUser {
                name,
                superuser,
                password,
            } => {
                if st.users.contains_key(name) {
                    return Err(DbError::Authz(format!("user '{name}' already exists")));
                }
                // Hash before inserting the user, so a hashing failure (e.g.
                // the password is somehow rejected) leaves no partial state —
                // either both the user and its credential land together, or
                // neither does.
                let hash = match password {
                    Some(pw) => Some(hash_password(pw)?),
                    None => None,
                };
                st.users.insert(name.clone(), *superuser);
                if let Some(hash) = hash {
                    st.credentials.insert(name.clone(), hash);
                }
            }
            AuthStmt::DropUser(name) => {
                if st.users.remove(name).is_none() {
                    return Err(DbError::Authz(format!("user '{name}' not found")));
                }
                st.memberships.remove(name);
                st.table_grants.remove(name);
                st.credentials.remove(name);
            }
            AuthStmt::CreateRole(name) => {
                if !st.roles.insert(name.clone()) {
                    return Err(DbError::Authz(format!("role '{name}' already exists")));
                }
            }
            AuthStmt::DropRole(name) => {
                if !st.roles.remove(name) {
                    return Err(DbError::Authz(format!("role '{name}' not found")));
                }
                st.memberships.remove(name);
                st.table_grants.remove(name);
                for roles in st.memberships.values_mut() {
                    roles.remove(name);
                }
            }
            AuthStmt::GrantPrivs {
                privs,
                table,
                grantee,
            } => {
                Self::require_grantee(&st, grantee)?;
                let entry = st
                    .table_grants
                    .entry(grantee.clone())
                    .or_default()
                    .entry(table.clone())
                    .or_default();
                for p in privs {
                    entry.insert(*p);
                }
            }
            AuthStmt::RevokePrivs {
                privs,
                table,
                grantee,
            } => {
                if let Some(t) = st
                    .table_grants
                    .get_mut(grantee)
                    .and_then(|g| g.get_mut(table))
                {
                    for p in privs {
                        t.remove(p);
                    }
                }
            }
            AuthStmt::GrantRole { role, grantee } => {
                if !st.roles.contains(role) {
                    return Err(DbError::Authz(format!("role '{role}' does not exist")));
                }
                Self::require_grantee(&st, grantee)?;
                st.memberships
                    .entry(grantee.clone())
                    .or_default()
                    .insert(role.clone());
            }
            AuthStmt::RevokeRole { role, grantee } => {
                if let Some(roles) = st.memberships.get_mut(grantee) {
                    roles.remove(role);
                }
            }
            // Policy DDL is routed to the catalog by `Engine::exec_auth_stmt`
            // before it ever reaches here. If somehow called directly, return
            // an internal error rather than panicking.
            AuthStmt::CreatePolicy(_) | AuthStmt::DropPolicy { .. } => {
                return Err(DbError::Authz(
                    "CREATE/DROP POLICY must be applied via Engine::exec_auth_stmt, not RoleStore::apply".into(),
                ));
            }
        }
        self.persist(&st)?;
        tracing::info!(?stmt, "auth DDL applied");
        Ok(())
    }

    fn require_grantee(st: &AuthState, grantee: &str) -> Result<()> {
        if st.users.contains_key(grantee) || st.roles.contains(grantee) {
            Ok(())
        } else {
            Err(DbError::Authz(format!(
                "grantee '{grantee}' is not a known user or role"
            )))
        }
    }
}

/// Compile-time proof the store is shareable on the `Engine`.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RoleStore>();
};

/// Detect + parse an auth-DDL statement. Returns `None` for non-auth SQL (which
/// flows to the normal parser). Errors only on a malformed auth statement.
pub fn parse_auth_stmt(sql: &str) -> Result<Option<AuthStmt>> {
    let trimmed = sql.trim().trim_end_matches(';');
    let toks: Vec<&str> = trimmed.split_whitespace().collect();
    if toks.len() < 2 {
        return Ok(None);
    }
    let kw = toks[0].to_ascii_uppercase();
    let kw2 = toks[1].to_ascii_uppercase();
    match (kw.as_str(), kw2.as_str()) {
        ("CREATE", "USER") => parse_create_user(trimmed, &toks).map(Some),
        ("DROP", "USER") => Ok(Some(AuthStmt::DropUser(ident(toks.get(2))?))),
        ("CREATE", "ROLE") => Ok(Some(AuthStmt::CreateRole(ident(toks.get(2))?))),
        ("DROP", "ROLE") => Ok(Some(AuthStmt::DropRole(ident(toks.get(2))?))),
        ("GRANT", _) => parse_grant_revoke(&toks, true).map(Some),
        ("REVOKE", _) => parse_grant_revoke(&toks, false).map(Some),
        ("CREATE", "POLICY") => parse_create_policy(trimmed).map(Some),
        ("DROP", "POLICY") => parse_drop_policy(&toks).map(Some),
        _ => Ok(None),
    }
}

/// `CREATE USER <name> [SUPERUSER] [PASSWORD '<pw>']` (item 121, A1).
///
/// `name`/`SUPERUSER` come from the whitespace-tokenised `toks` (as before);
/// the optional `PASSWORD` clause is scanned out of the raw `sql` string
/// instead, so a password containing spaces (or an escaped `''` literal
/// quote) parses correctly — unlike the rest of this token-based grammar, a
/// password is free-form text, not an identifier.
fn parse_create_user(sql: &str, toks: &[&str]) -> Result<AuthStmt> {
    let name = ident(toks.get(2))?;
    let superuser = toks
        .get(3)
        .map(|s| s.eq_ignore_ascii_case("SUPERUSER"))
        .unwrap_or(false);

    let upper = sql.to_ascii_uppercase();
    let password = match upper.find("PASSWORD") {
        Some(pw_pos) => Some(parse_quoted_password(&sql[pw_pos + "PASSWORD".len()..])?),
        None => None,
    };

    Ok(AuthStmt::CreateUser {
        name,
        superuser,
        password,
    })
}

/// Parse a single-quoted string literal starting somewhere in `s` (the first
/// `'` found), supporting a doubled `''` as an escaped literal quote
/// (SQL-standard string-literal escaping). Returns the unescaped contents.
fn parse_quoted_password(s: &str) -> Result<String> {
    let quote_start = s.find('\'').ok_or_else(|| {
        DbError::SqlParse("CREATE USER: PASSWORD clause must be a quoted string".into())
    })?;
    let rest = &s[quote_start + 1..];
    let mut out = String::new();
    let mut chars = rest.char_indices();
    let mut closed = false;
    while let Some((i, c)) = chars.next() {
        if c == '\'' {
            if rest[i + 1..].starts_with('\'') {
                out.push('\'');
                chars.next(); // consume the second quote of the doubled escape
            } else {
                closed = true;
                break;
            }
        } else {
            out.push(c);
        }
    }
    if !closed {
        return Err(DbError::SqlParse(
            "CREATE USER: unterminated PASSWORD string".into(),
        ));
    }
    if out.is_empty() {
        return Err(DbError::SqlParse(
            "CREATE USER: PASSWORD cannot be empty".into(),
        ));
    }
    Ok(out)
}

/// `CREATE POLICY <name> ON <table> FOR <op> USING (<predicate>)`
///
/// The USING clause may span multiple tokens (it is a SQL expression); we
/// find `USING` case-insensitively and take everything after `(` through the
/// final `)` as the raw predicate string.
fn parse_create_policy(sql: &str) -> Result<AuthStmt> {
    let upper = sql.to_ascii_uppercase();
    // Tokenised version for the keyword positions.
    let toks: Vec<&str> = sql.split_whitespace().collect();
    let upper_toks: Vec<String> = toks.iter().map(|t| t.to_ascii_uppercase()).collect();

    // Name is toks[2].
    let name = ident(toks.get(2))?;

    // ON position.
    let on_pos = upper_toks
        .iter()
        .position(|t| t == "ON")
        .ok_or_else(|| DbError::SqlParse("CREATE POLICY: missing ON clause".into()))?;
    let table = ident(toks.get(on_pos + 1))?;

    // FOR position (optional — defaults to ALL).
    let op = if let Some(for_pos) = upper_toks.iter().position(|t| t == "FOR") {
        let op_str = toks.get(for_pos + 1).ok_or_else(|| {
            DbError::SqlParse("CREATE POLICY: missing operation after FOR".into())
        })?;
        PolicyOp::parse(op_str).ok_or_else(|| {
            DbError::SqlParse(format!("CREATE POLICY: unknown operation '{op_str}'"))
        })?
    } else {
        PolicyOp::All
    };

    // USING clause: everything between the outermost `(` and `)` after the
    // USING keyword.
    let using_kw_pos = upper
        .find("USING")
        .ok_or_else(|| DbError::SqlParse("CREATE POLICY: missing USING clause".into()))?;
    let after_using = &sql[using_kw_pos + 5..]; // skip "USING"
    let open = after_using.find('(').ok_or_else(|| {
        DbError::SqlParse("CREATE POLICY: USING clause must be parenthesised".into())
    })?;
    let inner = &after_using[open + 1..];
    // Walk to find the matching close paren (depth-aware).
    let mut depth = 1usize;
    let mut close = None;
    for (i, ch) in inner.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let close = close
        .ok_or_else(|| DbError::SqlParse("CREATE POLICY: unclosed USING parenthesis".into()))?;
    let using_expr = inner[..close].trim().to_string();
    if using_expr.is_empty() {
        return Err(DbError::SqlParse(
            "CREATE POLICY: USING predicate cannot be empty".into(),
        ));
    }

    // Optional `WITH CHECK (expr)` after the USING clause (item-24 R-a).
    // We look in the portion of the original SQL that comes *after* the
    // closing `)` of the USING expression.
    let after_using_close_offset = (using_kw_pos + 5) + (open + 1) + close + 1;
    let remainder = if after_using_close_offset < sql.len() {
        &sql[after_using_close_offset..]
    } else {
        ""
    };
    let upper_rem = remainder.to_ascii_uppercase();
    let with_check_sql = if let Some(wc_pos) = upper_rem.find("WITH CHECK") {
        let after_wc = &remainder[wc_pos + 10..]; // skip "WITH CHECK"
        let wc_open = after_wc.find('(').ok_or_else(|| {
            DbError::SqlParse("CREATE POLICY: WITH CHECK clause must be parenthesised".into())
        })?;
        let wc_inner = &after_wc[wc_open + 1..];
        let mut depth = 1usize;
        let mut wc_close = None;
        for (i, ch) in wc_inner.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        wc_close = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let wc_close = wc_close.ok_or_else(|| {
            DbError::SqlParse("CREATE POLICY: unclosed WITH CHECK parenthesis".into())
        })?;
        let expr = wc_inner[..wc_close].trim().to_string();
        if expr.is_empty() {
            return Err(DbError::SqlParse(
                "CREATE POLICY: WITH CHECK predicate cannot be empty".into(),
            ));
        }
        Some(expr)
    } else {
        None
    };

    Ok(AuthStmt::CreatePolicy(PolicyDef {
        name,
        table,
        op,
        using_expr,
        with_check_sql,
    }))
}

/// `DROP POLICY <name> ON <table>`
fn parse_drop_policy(toks: &[&str]) -> Result<AuthStmt> {
    let name = ident(toks.get(2))?;
    let upper_toks: Vec<String> = toks.iter().map(|t| t.to_ascii_uppercase()).collect();
    let on_pos = upper_toks
        .iter()
        .position(|t| t == "ON")
        .ok_or_else(|| DbError::SqlParse("DROP POLICY: missing ON clause".into()))?;
    let table = ident(toks.get(on_pos + 1))?;
    Ok(AuthStmt::DropPolicy { name, table })
}

fn ident(tok: Option<&&str>) -> Result<String> {
    tok.map(|s| s.trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| DbError::SqlParse("auth statement: expected an identifier".into()))
}

/// `GRANT <priv,..|ALL> ON <table> TO <grantee>` /
/// `GRANT <role> TO <grantee>` (and the REVOKE ... FROM forms).
fn parse_grant_revoke(toks: &[&str], grant: bool) -> Result<AuthStmt> {
    // Find the ON / TO|FROM anchors.
    let upper: Vec<String> = toks.iter().map(|t| t.to_ascii_uppercase()).collect();
    let connector = if grant { "TO" } else { "FROM" };
    let conn_pos = upper
        .iter()
        .position(|t| t == connector)
        .ok_or_else(|| DbError::SqlParse(format!("GRANT/REVOKE: missing '{connector}' clause")))?;
    let grantee = ident(toks.get(conn_pos + 1))?;

    if let Some(on_pos) = upper.iter().position(|t| t == "ON") {
        // Table privileges: tokens[1..on_pos] are the privilege list.
        let table = ident(toks.get(on_pos + 1))?;
        let priv_str: String = toks[1..on_pos].join(" ");
        let privs = parse_priv_list(&priv_str)?;
        Ok(if grant {
            AuthStmt::GrantPrivs {
                privs,
                table,
                grantee,
            }
        } else {
            AuthStmt::RevokePrivs {
                privs,
                table,
                grantee,
            }
        })
    } else {
        // Role membership: `GRANT <role> TO <grantee>`.
        let role = ident(toks.get(1))?;
        Ok(if grant {
            AuthStmt::GrantRole { role, grantee }
        } else {
            AuthStmt::RevokeRole { role, grantee }
        })
    }
}

fn parse_priv_list(s: &str) -> Result<Vec<Privilege>> {
    if s.trim().eq_ignore_ascii_case("ALL") {
        return Ok(Privilege::all().to_vec());
    }
    let mut out = Vec::new();
    for part in s.split(',') {
        let p = part.trim();
        out.push(
            Privilege::parse(p)
                .ok_or_else(|| DbError::SqlParse(format!("unknown privilege '{p}'")))?,
        );
    }
    if out.is_empty() {
        return Err(DbError::SqlParse("GRANT: empty privilege list".into()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_auth_ddl() {
        assert_eq!(
            parse_auth_stmt("CREATE USER alice SUPERUSER").unwrap(),
            Some(AuthStmt::CreateUser {
                name: "alice".into(),
                superuser: true,
                password: None,
            })
        );
        assert_eq!(
            parse_auth_stmt("GRANT SELECT, INSERT ON accounts TO bob").unwrap(),
            Some(AuthStmt::GrantPrivs {
                privs: vec![Privilege::Select, Privilege::Insert],
                table: "accounts".into(),
                grantee: "bob".into()
            })
        );
        assert_eq!(
            parse_auth_stmt("GRANT analyst TO bob").unwrap(),
            Some(AuthStmt::GrantRole {
                role: "analyst".into(),
                grantee: "bob".into()
            })
        );
        assert_eq!(
            parse_auth_stmt("REVOKE ALL ON accounts FROM bob").unwrap(),
            Some(AuthStmt::RevokePrivs {
                privs: Privilege::all().to_vec(),
                table: "accounts".into(),
                grantee: "bob".into()
            })
        );
        assert!(parse_auth_stmt("SELECT * FROM t").unwrap().is_none());
    }

    #[test]
    fn privilege_resolution_through_roles() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "bob".into(),
                superuser: false,
                password: None,
            })
            .unwrap();
        store
            .apply(&AuthStmt::CreateRole("analyst".into()))
            .unwrap();
        store
            .apply(&AuthStmt::GrantPrivs {
                privs: vec![Privilege::Select],
                table: "accounts".into(),
                grantee: "analyst".into(),
            })
            .unwrap();
        // Bob has nothing yet.
        assert!(!store.has_privilege("bob", "accounts", Privilege::Select));
        // Grant the role → bob inherits SELECT.
        store
            .apply(&AuthStmt::GrantRole {
                role: "analyst".into(),
                grantee: "bob".into(),
            })
            .unwrap();
        assert!(store.has_privilege("bob", "accounts", Privilege::Select));
        assert!(!store.has_privilege("bob", "accounts", Privilege::Insert));
    }

    #[test]
    fn superuser_has_everything_and_persists() {
        let dir = tempdir().unwrap();
        {
            let store = RoleStore::open(dir.path()).unwrap();
            store
                .apply(&AuthStmt::CreateUser {
                    name: "root".into(),
                    superuser: true,
                    password: None,
                })
                .unwrap();
            assert!(store.has_privilege("root", "anything", Privilege::Delete));
        }
        // Reopen: the user persists.
        let store = RoleStore::open(dir.path()).unwrap();
        assert!(store.is_superuser("root"));
    }

    #[test]
    fn parse_create_user_with_password() {
        assert_eq!(
            parse_auth_stmt("CREATE USER alice PASSWORD 'hunter2'").unwrap(),
            Some(AuthStmt::CreateUser {
                name: "alice".into(),
                superuser: false,
                password: Some("hunter2".into()),
            })
        );
        // SUPERUSER + PASSWORD together, in that order.
        assert_eq!(
            parse_auth_stmt("CREATE USER root SUPERUSER PASSWORD 's3cret'").unwrap(),
            Some(AuthStmt::CreateUser {
                name: "root".into(),
                superuser: true,
                password: Some("s3cret".into()),
            })
        );
        // A password containing spaces and an escaped quote.
        assert_eq!(
            parse_auth_stmt("CREATE USER bob PASSWORD 'a b ''c'' d'").unwrap(),
            Some(AuthStmt::CreateUser {
                name: "bob".into(),
                superuser: false,
                password: Some("a b 'c' d".into()),
            })
        );
        // Malformed clauses are rejected rather than silently ignored.
        assert!(parse_auth_stmt("CREATE USER alice PASSWORD").is_err());
        assert!(parse_auth_stmt("CREATE USER alice PASSWORD ''").is_err());
        assert!(parse_auth_stmt("CREATE USER alice PASSWORD 'unterminated").is_err());
    }

    #[test]
    fn credential_store_hash_verify_and_persist() {
        let dir = tempdir().unwrap();
        {
            let store = RoleStore::open(dir.path()).unwrap();
            store
                .apply(&AuthStmt::CreateUser {
                    name: "carol".into(),
                    superuser: false,
                    password: Some("correct horse".into()),
                })
                .unwrap();
            // Correct password verifies.
            assert!(store.verify_password("carol", "correct horse"));
            // Wrong password fails.
            assert!(!store.verify_password("carol", "wrong password"));
            // Unknown user fails the same way (no panic, no special-case).
            assert!(!store.verify_password("nobody", "correct horse"));
            // A user with no stored credential (created without PASSWORD)
            // cannot log in via password at all.
            store
                .apply(&AuthStmt::CreateUser {
                    name: "dave".into(),
                    superuser: false,
                    password: None,
                })
                .unwrap();
            assert!(!store.verify_password("dave", ""));
            assert!(!store.verify_password("dave", "anything"));
        }
        // The credential survives a reopen (it's persisted control-plane
        // metadata, same file as users/roles/grants).
        let store = RoleStore::open(dir.path()).unwrap();
        assert!(store.verify_password("carol", "correct horse"));

        // The persisted roles.json is never plaintext: the raw file bytes
        // must not contain the password, only a PHC-formatted argon2id hash.
        let raw = std::fs::read_to_string(dir.path().join("roles.json")).unwrap();
        assert!(!raw.contains("correct horse"));
        assert!(raw.contains("$argon2id$"));
    }

    #[test]
    fn set_password_rust_api() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "erin".into(),
                superuser: false,
                password: None,
            })
            .unwrap();
        assert!(!store.verify_password("erin", "new-pw"));
        store.set_password("erin", "new-pw").unwrap();
        assert!(store.verify_password("erin", "new-pw"));
        assert!(!store.verify_password("erin", "wrong"));

        // Setting a password for a nonexistent user is an error.
        assert!(store.set_password("ghost", "x").is_err());
    }

    #[test]
    fn auth_state_debug_never_prints_password_hash() {
        let dir = tempdir().unwrap();
        let store = RoleStore::open(dir.path()).unwrap();
        store
            .apply(&AuthStmt::CreateUser {
                name: "frank".into(),
                superuser: false,
                password: Some("super-secret-value".into()),
            })
            .unwrap();
        let debug_str = format!("{:?}", store.lock());
        assert!(!debug_str.contains("super-secret-value"));
        assert!(!debug_str.contains("$argon2id$"));
        assert!(debug_str.contains("redacted"));

        // The AuthStmt itself must also never Debug-print the plaintext
        // (e.g. via `tracing::info!(?stmt, ..)` in `apply`).
        let stmt = AuthStmt::CreateUser {
            name: "gina".into(),
            superuser: false,
            password: Some("another-secret".into()),
        };
        let stmt_debug = format!("{stmt:?}");
        assert!(!stmt_debug.contains("another-secret"));
        assert!(stmt_debug.contains("redacted"));
    }
}
