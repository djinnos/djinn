//! Deterministic cached-vector embedding provider for memory-eval.
//!
//! Supplies embeddings through a cached `NoteEmbeddingProvider` keyed by
//! normalized content hash, ensuring repeated runs on the same commit and
//! fixtures produce byte-stable metric outputs.
//!
//! Implementation tracked by task csom (deterministic embedder contracts).
