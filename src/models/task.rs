use serde::{Deserialize, Serialize};

/// Task board work item. Minimal shape for event emission.
/// Full field set (status, priority, type, etc.) defined in feature 1la3.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
}
