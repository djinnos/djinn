//! Enriched-text rendering for `code_chunks`. Empty in PR B1 — B2 lands the
//! header/body composer and bumps `EMBEDDING_TEXT_VERSION`.

/// Bumped when the rendered chunk text changes shape. Used by the future
/// content-hash function so a rules change invalidates every existing meta
/// row in one pass.
pub const EMBEDDING_TEXT_VERSION: &str = "v0";
