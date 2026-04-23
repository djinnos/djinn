use serde::{Deserialize, Serialize};

/// A registered project. Identity is `(github_owner, github_repo)`;
/// the filesystem path is derived per-process from
/// `djinn_core::paths::project_dir` and is not persisted.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Project {
    pub id: String,
    pub name: String,
    pub github_owner: String,
    pub github_repo: String,
    pub created_at: String,
    pub target_branch: String,
    pub auto_merge: bool,
    pub sync_enabled: bool,
    pub sync_remote: Option<String>,
}

impl Project {
    /// Canonical `"owner/repo"` handle. This is the shape MCP tools
    /// accept as an alternative to the UUID `id`.
    pub fn slug(&self) -> String {
        format!("{}/{}", self.github_owner, self.github_repo)
    }
}
