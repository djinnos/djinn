//! Deterministic cached-vector embedding provider for memory-eval.
//!
//! Supplies embeddings through a cached [`NoteEmbeddingProvider`] keyed by
//! normalized content hash, ensuring repeated runs on the same commit and
//! fixtures produce byte-stable metric outputs.
//!
//! # Design
//!
//! The provider hashes normalised note text with SHA-256 and expands the
//! digest into a fixed-dimension L2-normalised `f32` vector.  No network
//! calls, no LLM invocations, and no randomness — the same normalised
//! input always yields the exact same vector and model metadata.
//!
//! The [`NoteEmbeddingProvider`] trait defined here mirrors the contract in
//! `djinn_db::repositories::note::embeddings::NoteEmbeddingProvider` so the
//! deterministic provider is a drop-in when the fixture loader (task qmzw)
//! bridges the two.

// This is a binary-crate module; these public types are API contracts for
// downstream modules (qmzw, zd4o) and will be consumed in future tasks.
#![allow(dead_code)]

use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Trait & types — mirrors djinn-db contract
// ---------------------------------------------------------------------------

/// Model version reported by the deterministic provider.
pub const DETERMINISTIC_MODEL_VERSION: &str = "deterministic-v1";

/// Default embedding dimension (matches common sentence-transformer output).
pub const DEFAULT_EMBEDDING_DIMENSION: usize = 384;

/// Trait matching the `NoteEmbeddingProvider` contract from
/// `djinn_db::repositories::note::embeddings`.
///
/// Defined locally to avoid pulling the heavy djinn-db dependency graph
/// (sqlx, qdrant, etc.) into this lightweight benchmark crate.
pub trait NoteEmbeddingProvider: Send + Sync {
    /// Return the model version string embedded alongside every vector.
    fn model_version(&self) -> String;

    /// Produce an embedding for the given text.
    async fn embed_note(&self, text: &str) -> Result<EmbeddedNote, String>;
}

/// Result of embedding a note's text.
///
/// Mirrors `djinn_db::repositories::note::EmbeddedNote`.
#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddedNote {
    pub values: Vec<f32>,
    pub model_version: String,
}

// ---------------------------------------------------------------------------
// Deterministic provider
// ---------------------------------------------------------------------------

/// Deterministic cached-vector embedding provider.
///
/// Generates embeddings by hashing normalised text content with SHA-256 and
/// expanding the hash into a fixed-dimension float vector.  The process is
/// fully deterministic: identical (normalised) input always produces
/// byte-identical output, with no network or LLM calls.
pub struct DeterministicEmbeddingProvider {
    dimension: usize,
}

impl DeterministicEmbeddingProvider {
    /// Create a provider with the given embedding dimension.
    pub fn new(dimension: usize) -> Self {
        assert!(dimension > 0, "embedding dimension must be > 0");
        Self { dimension }
    }

    /// Create a provider with the standard default dimension (384).
    pub fn with_default_dimension() -> Self {
        Self::new(DEFAULT_EMBEDDING_DIMENSION)
    }

    /// Return the configured embedding dimension.
    pub fn dimension(&self) -> usize {
        self.dimension
    }
}

impl NoteEmbeddingProvider for DeterministicEmbeddingProvider {
    fn model_version(&self) -> String {
        DETERMINISTIC_MODEL_VERSION.to_owned()
    }

    async fn embed_note(&self, text: &str) -> Result<EmbeddedNote, String> {
        let normalized = normalize_text(text);
        let content_hash = Sha256::digest(normalized.as_bytes());
        let values = expand_hash_to_vector(&content_hash, self.dimension);
        Ok(EmbeddedNote {
            values,
            model_version: DETERMINISTIC_MODEL_VERSION.to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Normalise text for deterministic hashing.
///
/// Mirrors `djinn_db::note_hash::normalize_note_content`:
/// - converts CRLF and CR to LF
/// - trims leading/trailing whitespace
pub(crate) fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

/// Compute a hex SHA-256 content hash of normalised text.
///
/// Matches the output of `djinn_db::note_hash::note_content_hash` for the
/// same input, which can be used as a stable cache key.
pub fn deterministic_content_hash(text: &str) -> String {
    let normalized = normalize_text(text);
    let digest = Sha256::digest(normalized.as_bytes());
    format!("{digest:x}")
}

/// Expand a SHA-256 digest into a fixed-dimension L2-normalised `f32` vector.
///
/// Each round appends a big-endian counter to the original digest and hashes
/// again, yielding 32 bytes (8 × f32) per round.  The resulting floats are
/// mapped to the range `[-1, 1]` and then L2-normalised so the vector lives
/// on the unit hypersphere (suitable for cosine-similarity ranking).
fn expand_hash_to_vector(hash: &[u8], dimension: usize) -> Vec<f32> {
    let mut values = Vec::with_capacity(dimension);
    let mut counter: u32 = 0;

    while values.len() < dimension {
        let mut input = Vec::with_capacity(hash.len() + 4);
        input.extend_from_slice(hash);
        input.extend_from_slice(&counter.to_be_bytes());
        let expanded = Sha256::digest(&input);

        for chunk in expanded.chunks_exact(4) {
            if values.len() >= dimension {
                break;
            }
            let bits = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            // Map u32 → f32 in [-1, 1]
            let value = (bits as f32 / u32::MAX as f32) * 2.0 - 1.0;
            values.push(value);
        }
        counter += 1;
    }

    // L2-normalise so cosine similarity works as expected.
    let norm: f32 = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut values {
            *v /= norm;
        }
    }
    values
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- determinism & equality -------------------------------------------

    #[tokio::test]
    async fn identical_input_produces_identical_vectors() {
        let provider = DeterministicEmbeddingProvider::new(64);
        let text = "Hello, world!";
        let a = provider.embed_note(text).await.unwrap();
        let b = provider.embed_note(text).await.unwrap();
        assert_eq!(a, b, "same input must yield byte-identical EmbeddedNote");
        assert_eq!(a.model_version, DETERMINISTIC_MODEL_VERSION);
    }

    #[tokio::test]
    async fn repeated_calls_are_stable() {
        let provider = DeterministicEmbeddingProvider::with_default_dimension();
        let text = "stability check with unicode: \u{1f600}";
        let first = provider.embed_note(text).await.unwrap();
        for _ in 0..10 {
            let again = provider.embed_note(text).await.unwrap();
            assert_eq!(first.values, again.values);
            assert_eq!(first.model_version, again.model_version);
        }
    }

    // -- normalisation equivalence ----------------------------------------

    #[tokio::test]
    async fn crlf_and_lf_are_equivalent() {
        let provider = DeterministicEmbeddingProvider::new(64);
        let a = provider.embed_note("Hello\r\nWorld").await.unwrap();
        let b = provider.embed_note("Hello\nWorld").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn standalone_cr_normalizes_to_lf() {
        let provider = DeterministicEmbeddingProvider::new(64);
        let a = provider.embed_note("a\rb").await.unwrap();
        let b = provider.embed_note("a\nb").await.unwrap();
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn leading_trailing_whitespace_is_trimmed() {
        let provider = DeterministicEmbeddingProvider::new(64);
        let a = provider.embed_note("  content  ").await.unwrap();
        let b = provider.embed_note("content").await.unwrap();
        assert_eq!(a, b);
    }

    // -- distinct inputs → distinct vectors --------------------------------

    #[tokio::test]
    async fn different_content_produces_different_vectors() {
        let provider = DeterministicEmbeddingProvider::new(64);
        let a = provider.embed_note("alpha").await.unwrap();
        let b = provider.embed_note("beta").await.unwrap();
        assert_ne!(
            a.values, b.values,
            "different content must produce distinct vectors"
        );
        // Same model version regardless of content.
        assert_eq!(a.model_version, b.model_version);
    }

    #[tokio::test]
    async fn similar_but_distinct_texts_diverge() {
        let provider = DeterministicEmbeddingProvider::new(64);
        let a = provider.embed_note("the cat sat on the mat").await.unwrap();
        let b = provider
            .embed_note("the cat sat on the mat.")
            .await
            .unwrap();
        assert_ne!(a.values, b.values);
    }

    // -- model metadata ---------------------------------------------------

    #[tokio::test]
    async fn model_version_is_constant() {
        let provider = DeterministicEmbeddingProvider::new(16);
        let result = provider.embed_note("anything").await.unwrap();
        assert_eq!(result.model_version, DETERMINISTIC_MODEL_VERSION);
    }

    #[test]
    fn model_version_method_matches_struct_field() {
        let provider = DeterministicEmbeddingProvider::new(16);
        assert_eq!(provider.model_version(), DETERMINISTIC_MODEL_VERSION);
    }

    // -- dimension ---------------------------------------------------------

    #[tokio::test]
    async fn vector_dimension_matches_config() {
        for dim in [1, 8, 16, 64, 128, 384] {
            let provider = DeterministicEmbeddingProvider::new(dim);
            let result = provider.embed_note("dim test").await.unwrap();
            assert_eq!(result.values.len(), dim, "dimension mismatch for dim={dim}");
        }
    }

    #[tokio::test]
    async fn default_dimension_is_384() {
        let provider = DeterministicEmbeddingProvider::with_default_dimension();
        assert_eq!(provider.dimension(), DEFAULT_EMBEDDING_DIMENSION);
        let result = provider.embed_note("test").await.unwrap();
        assert_eq!(result.values.len(), DEFAULT_EMBEDDING_DIMENSION);
    }

    // -- L2 normalisation --------------------------------------------------

    #[tokio::test]
    async fn vectors_are_l2_normalized() {
        let provider = DeterministicEmbeddingProvider::with_default_dimension();
        for text in &["hello", "world", "a longer piece of text for testing"] {
            let result = provider.embed_note(text).await.unwrap();
            let norm: f32 = result.values.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-5,
                "expected L2 norm ~1.0, got {norm} for text {:?}",
                text
            );
        }
    }

    // -- content hash utility ----------------------------------------------

    #[test]
    fn content_hash_is_stable() {
        let a = deterministic_content_hash("hello world");
        let b = deterministic_content_hash("hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn content_hash_normalizes_like_provider() {
        let a = deterministic_content_hash("hello\r\nworld");
        let b = deterministic_content_hash("hello\nworld");
        assert_eq!(a, b);
    }

    #[test]
    fn content_hash_differs_for_distinct_text() {
        let a = deterministic_content_hash("alpha");
        let b = deterministic_content_hash("beta");
        assert_ne!(a, b);
    }

    // -- normalize_text helper ---------------------------------------------

    #[test]
    fn normalize_text_handles_line_endings() {
        assert_eq!(normalize_text("hello\r\nworld"), "hello\nworld");
        assert_eq!(normalize_text("a\rb"), "a\nb");
    }

    #[test]
    fn normalize_text_trims_whitespace() {
        assert_eq!(normalize_text("\r\n  hello \r\n"), "hello");
        assert_eq!(normalize_text("  \t  "), "");
    }

    // -- expand_hash_to_vector internal ------------------------------------

    #[test]
    fn expand_determinism() {
        let hash = Sha256::digest(b"test input");
        let a = expand_hash_to_vector(&hash, 32);
        let b = expand_hash_to_vector(&hash, 32);
        assert_eq!(a, b);
    }

    #[test]
    fn expand_different_hashes_diverge() {
        let h1 = Sha256::digest(b"input a");
        let h2 = Sha256::digest(b"input b");
        let v1 = expand_hash_to_vector(&h1, 32);
        let v2 = expand_hash_to_vector(&h2, 32);
        assert_ne!(v1, v2);
    }

    #[test]
    fn expand_small_dimensions() {
        let hash = Sha256::digest(b"test");
        // Even a single-element vector should work.
        let v = expand_hash_to_vector(&hash, 1);
        assert_eq!(v.len(), 1);
    }
}
