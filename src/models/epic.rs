use serde::{Deserialize, Serialize};

/// Top-level grouping entity (separate from tasks). Minimal shape for event emission.
/// Full field set defined in a later feature.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Epic {
    pub id: String,
}
