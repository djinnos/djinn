use serde::{Deserialize, Serialize};

/// A stored credential entry — never exposes the raw key value.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Credential {
    pub id: String,
    pub provider_id: String,
    /// Env-var style name, e.g. `ANTHROPIC_API_KEY`.
    pub key_name: String,
    pub created_at: String,
    pub updated_at: String,
}
