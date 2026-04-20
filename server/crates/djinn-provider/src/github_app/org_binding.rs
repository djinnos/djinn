//! Env-loaded GitHub-org binding for a Djinn deployment.
//!
//! Counterpart to [`AppConfig`](super::AppConfig): where `AppConfig` carries
//! the *App* identity (App ID, slug, OAuth client, private key), [`OrgBinding`]
//! carries the *deployment-to-org* binding (which org this Djinn instance is
//! locked to, and which installation grants it server-side access).
//!
//! Like `AppConfig`, this loads exclusively from environment variables — the
//! K8s Secret mounted into the server Pod. The previous in-DB `org_config`
//! row remains for back-compat reads but is no longer the source of truth;
//! when both env and DB are present, env wins.
//!
//! # Environment
//! - [`ENV_ORG_LOGIN`] (`GITHUB_ORG_LOGIN`) — required.
//! - [`ENV_INSTALLATION_ID`] (`GITHUB_INSTALLATION_ID`) — required, numeric.
//! - [`ENV_ORG_ID`] (`GITHUB_ORG_ID`) — optional, numeric. May be fetched from
//!   the GitHub API on demand when absent.

/// Env var: GitHub org login (e.g. `acme`) this deployment is bound to.
pub const ENV_ORG_LOGIN: &str = "GITHUB_ORG_LOGIN";
/// Env var: numeric GitHub App installation id granting server-side access.
pub const ENV_INSTALLATION_ID: &str = "GITHUB_INSTALLATION_ID";
/// Env var: numeric GitHub org id (optional; can be fetched on demand).
pub const ENV_ORG_ID: &str = "GITHUB_ORG_ID";

/// Resolved deployment-to-org binding loaded from env.
///
/// Cheap to clone (small `String` + two integers); held inside `AppState`
/// behind the same `RwLock` pattern as [`AppConfig`](super::AppConfig).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgBinding {
    /// GitHub org login (e.g. `acme`). Used in API paths like
    /// `/orgs/{org}/members` and for the membership gate.
    pub org_login: String,
    /// GitHub App installation id; passed to
    /// [`get_installation_token`](super::installations::get_installation_token)
    /// to mint installation access tokens.
    pub installation_id: u64,
    /// Numeric GitHub org id. Optional in env; can be fetched from
    /// `GET /orgs/{org}` on demand when absent.
    pub org_id: Option<i64>,
}

impl OrgBinding {
    /// Load from `GITHUB_ORG_LOGIN` + `GITHUB_INSTALLATION_ID` (+ optional
    /// `GITHUB_ORG_ID`). Returns `None` if either of the two required fields
    /// is absent, empty, or unparseable.
    pub fn load_from_env() -> Option<Self> {
        let org_login = std::env::var(ENV_ORG_LOGIN).ok()?;
        let org_login = org_login.trim();
        if org_login.is_empty() {
            return None;
        }

        let installation_id_raw = std::env::var(ENV_INSTALLATION_ID).ok()?;
        let installation_id = installation_id_raw.trim().parse::<u64>().ok()?;

        let org_id = std::env::var(ENV_ORG_ID)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok());

        Some(Self {
            org_login: org_login.to_string(),
            installation_id,
            org_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Serialise env mutations across tests in this module — `std::env::set_var`
    // is process-global and Rust runs unit tests on a thread pool by default.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_all() {
        // SAFETY: the ENV_LOCK guard held by the caller serialises mutation.
        unsafe {
            std::env::remove_var(ENV_ORG_LOGIN);
            std::env::remove_var(ENV_INSTALLATION_ID);
            std::env::remove_var(ENV_ORG_ID);
        }
    }

    #[test]
    fn load_returns_some_when_both_required_set() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        unsafe {
            std::env::set_var(ENV_ORG_LOGIN, "acme");
            std::env::set_var(ENV_INSTALLATION_ID, "12345");
            std::env::set_var(ENV_ORG_ID, "987");
        }
        let b = OrgBinding::load_from_env().expect("binding should load");
        assert_eq!(b.org_login, "acme");
        assert_eq!(b.installation_id, 12345);
        assert_eq!(b.org_id, Some(987));
        clear_all();
    }

    #[test]
    fn load_org_id_optional() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        unsafe {
            std::env::set_var(ENV_ORG_LOGIN, "acme");
            std::env::set_var(ENV_INSTALLATION_ID, "42");
        }
        let b = OrgBinding::load_from_env().expect("binding should load");
        assert_eq!(b.org_id, None);
        clear_all();
    }

    #[test]
    fn load_returns_none_when_login_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        unsafe {
            std::env::set_var(ENV_INSTALLATION_ID, "42");
        }
        assert!(OrgBinding::load_from_env().is_none());
        clear_all();
    }

    #[test]
    fn load_returns_none_when_login_blank() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        unsafe {
            std::env::set_var(ENV_ORG_LOGIN, "   ");
            std::env::set_var(ENV_INSTALLATION_ID, "42");
        }
        assert!(OrgBinding::load_from_env().is_none());
        clear_all();
    }

    #[test]
    fn load_returns_none_when_installation_id_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        unsafe {
            std::env::set_var(ENV_ORG_LOGIN, "acme");
        }
        assert!(OrgBinding::load_from_env().is_none());
        clear_all();
    }

    #[test]
    fn load_returns_none_when_installation_id_unparseable() {
        let _g = ENV_LOCK.lock().unwrap();
        clear_all();
        unsafe {
            std::env::set_var(ENV_ORG_LOGIN, "acme");
            std::env::set_var(ENV_INSTALLATION_ID, "not-a-number");
        }
        assert!(OrgBinding::load_from_env().is_none());
        clear_all();
    }
}
