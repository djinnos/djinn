use serde::{Deserialize, Serialize};

/// A compact search result from proposals FTS with score and content snippet.
///
/// Mirrors [`NoteSearchResult`] for the proposal entity so the unified search
/// surface can interleave note and proposal hits with a consistent shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProposalSearchResult {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
    /// HTML snippet with `<b>…</b>` highlights around matched terms (from
    /// `ts_headline` on Postgres; `substr(body, 1, 200)` on SQLite).
    pub snippet: String,
    /// RRF fusion score (higher = more relevant).
    pub score: f64,
}
