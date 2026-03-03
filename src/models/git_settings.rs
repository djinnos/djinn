use serde::{Deserialize, Serialize};

/// Per-project git configuration (GIT-08, CFG-03).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitSettings {
    /// Branch that task branches are created from and merged back into.
    pub target_branch: String,
}

impl Default for GitSettings {
    fn default() -> Self {
        Self { target_branch: "main".into() }
    }
}
