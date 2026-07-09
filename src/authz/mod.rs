// Users, roles, and privileges (P6.e).
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
// GRANT, REVOKE) and schema DDL require superuser in v1; data statements
// (SELECT/INSERT/UPDATE/DELETE) require the matching privilege on the table.
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
//
// Persisted to `roles.json` (control-plane metadata, so `serde` is fine per
// CLAUDE.md §4). `Send + Sync` for the shared `Engine`.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};

use crate::error::{DbError, Result};

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
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AuthState {
    /// username → superuser?
    users: BTreeMap<String, bool>,
    roles: BTreeSet<String>,
    /// grantee (user or role) → roles it is a member of.
    memberships: BTreeMap<String, BTreeSet<String>>,
    /// grantee → table → privileges.
    table_grants: BTreeMap<String, BTreeMap<String, BTreeSet<Privilege>>>,
}

/// A parsed auth-DDL statement.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthStmt {
    CreateUser {
        name: String,
        superuser: bool,
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

    /// Apply a parsed auth-DDL statement. The caller must have already checked
    /// the actor is a superuser.
    pub fn apply(&self, stmt: &AuthStmt) -> Result<()> {
        let mut st = self.lock();
        match stmt {
            AuthStmt::CreateUser { name, superuser } => {
                if st.users.contains_key(name) {
                    return Err(DbError::Authz(format!("user '{name}' already exists")));
                }
                st.users.insert(name.clone(), *superuser);
            }
            AuthStmt::DropUser(name) => {
                if st.users.remove(name).is_none() {
                    return Err(DbError::Authz(format!("user '{name}' not found")));
                }
                st.memberships.remove(name);
                st.table_grants.remove(name);
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
        ("CREATE", "USER") => {
            let name = ident(toks.get(2))?;
            let superuser = toks
                .get(3)
                .map(|s| s.eq_ignore_ascii_case("SUPERUSER"))
                .unwrap_or(false);
            Ok(Some(AuthStmt::CreateUser { name, superuser }))
        }
        ("DROP", "USER") => Ok(Some(AuthStmt::DropUser(ident(toks.get(2))?))),
        ("CREATE", "ROLE") => Ok(Some(AuthStmt::CreateRole(ident(toks.get(2))?))),
        ("DROP", "ROLE") => Ok(Some(AuthStmt::DropRole(ident(toks.get(2))?))),
        ("GRANT", _) => parse_grant_revoke(&toks, true).map(Some),
        ("REVOKE", _) => parse_grant_revoke(&toks, false).map(Some),
        _ => Ok(None),
    }
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
                superuser: true
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
                })
                .unwrap();
            assert!(store.has_privilege("root", "anything", Privilege::Delete));
        }
        // Reopen: the user persists.
        let store = RoleStore::open(dir.path()).unwrap();
        assert!(store.is_superuser("root"));
    }
}
