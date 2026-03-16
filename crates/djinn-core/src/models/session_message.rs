use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct SessionMessage {
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub content_json: String,
    pub token_count: Option<i64>,
    pub created_at: String,
}
