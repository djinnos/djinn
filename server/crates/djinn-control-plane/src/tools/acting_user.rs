//! Shared admin gating + "act-as" identity resolution for MCP tools.
use djinn_core::auth_context::current_user_id;
use djinn_db::{Database, UserRepository};

/// Admin gate. `Ok(())` when the acting user is an admin OR when there is no
/// user context at all (local single-user / background trusted path, matching
/// the credential-repo convention). An authenticated non-admin is rejected.
pub(crate) async fn require_admin(db: &Database) -> Result<(), String> {
    let Some(uid) = current_user_id() else {
        return Ok(());
    };
    match UserRepository::new(db.clone()).get_by_id(&uid).await {
        Ok(Some(u)) if u.is_admin => Ok(()),
        Ok(_) => Err("admin privileges are required".to_string()),
        Err(e) => Err(format!("admin check failed: {e}")),
    }
}

/// The acting user's proposal capabilities. Resolved from `users.role` +
/// `is_admin`. `None` means there's no user context (trusted/local/system
/// path) — callers treat that as "allow", matching `require_admin`.
pub(crate) struct ActingCaps {
    pub user_id: String,
    pub role: String,
    pub is_admin: bool,
}

impl ActingCaps {
    /// Who may give a `scoped` (product) vs `technical` (engineering) sign-off.
    /// Engineers/admins may also scope so all-engineer teams aren't deadlocked.
    pub(crate) fn can_signoff(&self, kind: &str) -> bool {
        if self.is_admin {
            return true;
        }
        match kind {
            "scoped" => matches!(self.role.as_str(), "pm" | "engineer"),
            "technical" => self.role == "engineer",
            _ => false,
        }
    }

    /// Who may directly edit the spec / accept suggestions: the author, a PM,
    /// an engineer, or an admin.
    pub(crate) fn can_edit(&self, is_author: bool) -> bool {
        self.is_admin || is_author || matches!(self.role.as_str(), "pm" | "engineer")
    }

    /// Who may kick off a build (graduate): engineers and admins.
    pub(crate) fn can_kickoff(&self) -> bool {
        self.is_admin || self.role == "engineer"
    }
}

/// Load the acting user's capabilities, or `None` when unauthenticated.
pub(crate) async fn acting_caps(db: &Database) -> Result<Option<ActingCaps>, String> {
    let Some(uid) = current_user_id() else {
        return Ok(None);
    };
    match UserRepository::new(db.clone()).get_by_id(&uid).await {
        Ok(Some(u)) => Ok(Some(ActingCaps {
            user_id: u.id,
            role: u.role,
            is_admin: u.is_admin,
        })),
        Ok(None) => Ok(None),
        Err(e) => Err(format!("user lookup failed: {e}")),
    }
}

/// Resolve the effective user for an admin "act-as" operation. `None` target →
/// the acting user (`current_user_id()`). `Some(t)` → requires the caller to be
/// admin and verifies that `t` is a real `users.id` before returning it, letting
/// an admin read/write another user's per-user config (e.g. another target user
/// that can't self-configure). Rejecting unknown targets here keeps invalid UI
/// sentinels or stale user IDs from reaching writes guarded by user FKs.
pub(crate) async fn resolve_effective_user(
    db: &Database,
    target_user_id: Option<&str>,
) -> Result<Option<String>, String> {
    match target_user_id {
        None => Ok(current_user_id()),
        Some(t) => {
            require_admin(db).await?;
            match UserRepository::new(db.clone()).get_by_id(t).await {
                Ok(Some(_)) => Ok(Some(t.to_string())),
                Ok(None) => Err(format!("target user '{t}' was not found")),
                Err(e) => Err(format!("target user lookup failed: {e}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::auth_context::SESSION_USER_ID;

    async fn seed_user(db: &Database, github_id: i64, login: &str) -> String {
        UserRepository::new(db.clone())
            .upsert_from_github(github_id, login, None, None)
            .await
            .expect("seed user")
            .id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_unknown_target_is_rejected_before_fk_backed_writes() {
        let db = Database::open_in_memory().expect("open in-memory database");
        let admin_id = seed_user(&db, 90_001, "acting-admin").await;
        UserRepository::new(db.clone())
            .set_admin_status(&admin_id, true)
            .await
            .expect("promote acting user");

        let result = SESSION_USER_ID
            .scope(Some(admin_id), async {
                resolve_effective_user(&db, Some("__self__")).await
            })
            .await;

        assert_eq!(
            result,
            Err("target user '__self__' was not found".to_string())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_existing_target_is_returned_for_admin() {
        let db = Database::open_in_memory().expect("open in-memory database");
        let admin_id = seed_user(&db, 90_002, "acting-admin-existing").await;
        let target_id = seed_user(&db, 90_003, "oauth-target").await;
        UserRepository::new(db.clone())
            .set_admin_status(&admin_id, true)
            .await
            .expect("promote acting user");

        let result = SESSION_USER_ID
            .scope(Some(admin_id), async {
                resolve_effective_user(&db, Some(&target_id)).await
            })
            .await;

        assert_eq!(result, Ok(Some(target_id)));
    }
}
