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

/// Resolve the effective user for an admin "act-as" operation. `None` target →
/// the acting user (`current_user_id()`). `Some(t)` → requires the caller to be
/// admin and returns `t`, letting an admin read/write another user's per-user
/// config (e.g. the automation service user, which can't self-configure).
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
