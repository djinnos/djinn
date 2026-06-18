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
/// admin and returns `t`, letting an admin read/write another user's per-user
/// config (e.g. another target user that can't self-configure).
pub(crate) async fn resolve_effective_user(
    db: &Database,
    target_user_id: Option<&str>,
) -> Result<Option<String>, String> {
    match target_user_id {
        None => Ok(current_user_id()),
        Some(t) => {
            require_admin(db).await?;
            Ok(Some(t.to_string()))
        }
    }
}
