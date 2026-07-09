//! Fixture loading and schema contracts for the Phase 1 memory-eval corpus.
//!
//! Committed JSONL fixtures live under `fixtures/` and contain notes, queries,
//! labels, and bad-case rows. This module will provide the loader that feeds
//! them into the dedicated Postgres schema for benchmark runs.
//!
//! Implementation tracked by task 27tl (fixture schema contracts)
//! and qmzw (real Postgres fixture loader).
