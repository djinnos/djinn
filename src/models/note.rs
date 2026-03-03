use serde::{Deserialize, Serialize};

/// Knowledge base note with FTS5 indexing. Minimal shape for event emission.
/// Full field set (title, content, type, wikilinks, etc.) defined in feature kt1l.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
}
