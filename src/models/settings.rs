use serde::{Deserialize, Serialize};

/// A key-value setting persisted in the `settings` table.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Setting {
    pub key: String,
    pub value: String,
    pub updated_at: String,
}
