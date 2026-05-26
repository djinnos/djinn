use serde::{Deserialize, Serialize};

/// A stored credential entry — never exposes the raw key value.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Credential {
    pub id: String,
    pub provider_id: String,
    /// Env-var style name, e.g. `ANTHROPIC_API_KEY`.
    pub key_name: String,
    /// Owning user (`users.id`). `None` designates an org-shared / fallback
    /// credential used by any acting user who has no personal credential for
    /// the same `key_name`. Queries that don't select the column (legacy
    /// projections) default it to `None` via `sqlx(default)`.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub owner_user_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
