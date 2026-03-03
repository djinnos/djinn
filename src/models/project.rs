use serde::{Deserialize, Serialize};

/// A registered project.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: String,
}
