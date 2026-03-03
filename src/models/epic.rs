use serde::{Deserialize, Serialize};

/// Top-level grouping entity with a simplified open→closed lifecycle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Epic {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub description: String,
    pub emoji: String,
    pub color: String,
    pub status: String,
    pub owner: String,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
}
