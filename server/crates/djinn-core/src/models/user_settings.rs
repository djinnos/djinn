use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct UserSettings {
    pub user_id: String,
    pub auto_approve_prs: bool,
    /// Per-user ordered model selection (highest priority first), full
    /// `provider/model` ids. `None` = no explicit selection → callers fall back
    /// to the global deployment model list. Persisted as a JSON-array TEXT
    /// column (`user_settings.models`, migration 31).
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub models: Option<Vec<String>>,
    pub created_at: String,
    pub updated_at: String,
}

impl UserSettings {
    /// All-defaults-off row for users who have never written a setting. Lets
    /// the read path return `Some(defaults)` without inserting a row, so a
    /// `get` call for a freshly-created user doesn't pollute the table.
    pub fn defaults_for(user_id: &str) -> Self {
        Self {
            user_id: user_id.to_string(),
            auto_approve_prs: false,
            models: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
}
