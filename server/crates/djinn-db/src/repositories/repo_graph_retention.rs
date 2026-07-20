//! Publication-safe bounded graph retention (epic z8ch, Wave 1 item 1).
//!
//! For each project sweep, this module recomputes survivors under the
//! migration-125 project publication advisory lock
//! (`pg_advisory_xact_lock(hashtextextended(project_id, 0))`), defines
//! survivors as the current generation union the newest configured N
//! `publish_seq` generations, and deletes up to 25 non-survivor generations
//! per transaction. It reuses the canonical nonblocking exclusive generation
//! stream pin (see [`generation_stream_pin_key`],
//! [`try_acquire_generation_stream_pin_exclusive`],
//! [`release_generation_stream_pin_exclusive`]) so actively-pinned
//! generations are skipped without waiting, and relies on the existing FK
//! cascades for artifact/chunk removal.
//!
//! Lock order preserved by this module:
//! project advisory lock → compatibility row when publication is the actor →
//! current pointer → candidate row → generation stream pin.
//!
//! Bounded deadlock/serialization retry ([`retry_on_serialization_failure`])
//! is applied as defense in depth; correctness does not depend on choosing
//! a deadlock victim because the lock order is consistent and the stream pin is
//! nonblocking.

use crate::database::Database;
use crate::error::{DbError, DbResult};
use crate::repositories::repo_graph_generation::{
    self, GenerationStreamPinKey, release_generation_stream_pin_exclusive,
    try_acquire_generation_stream_pin_exclusive,
};
use crate::retry::{DEFAULT_MAX_TX_RETRIES, retry_on_serialization_failure};
use sqlx::{Acquire, Postgres, Transaction};

/// Maximum number of non-survivor generations a single sweep transaction may
/// delete. A sweep may scan past actively-pinned rows to fill a batch up to
/// this many actual candidates.
pub const MAX_RETENTION_BATCH: usize = 25;

/// Default survivor window: the newest N `publish_seq` generations (in
/// addition to the always-retained current generation).
pub const DEFAULT_RETENTION_HISTORY_N: usize = 3;

/// Minimum valid `history_n` value.
pub const MIN_RETENTION_HISTORY_N: usize = 1;

/// Keyset page size for the non-survivor scan. The scan pages through
/// generations oldest-`publish_seq`-first, continuing past actively-pinned
/// rows until [`MAX_RETENTION_BATCH`] actual candidates are collected or the
/// non-survivor set is exhausted. This is a *page* size, not a total cap —
/// the scan keeps fetching the next page until it has enough candidates.
const RETENTION_SCAN_PAGE_SIZE: i64 = 512;

/// Retention operating mode. The leader runner selects `off`/`dry_run`/`delete`
/// from validated configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionMode {
    /// Retention sweeps are disabled.
    Off,
    /// Compute and report candidates without deleting any rows.
    DryRun,
    /// Delete non-survivor generations (bounded to [`MAX_RETENTION_BATCH`]).
    Delete,
}

/// Request for a single project retention sweep.
#[derive(Clone, Copy, Debug)]
pub struct RetentionSweepRequest<'a> {
    pub project_id: &'a str,
    pub mode: RetentionMode,
    /// Number of newest `publish_seq` generations to keep (in addition to the
    /// current generation). Clamped to at least [`MIN_RETENTION_HISTORY_N`].
    pub history_n: usize,
}

/// A bounded, fixed reason a candidate generation was skipped (not deleted).
///
/// This is a small fixed enum rather than a free-form string so telemetry
/// cardinality stays bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionSkipClass {
    /// The candidate's stream pin was held by an active reader.
    ActiveStreamPin,
    /// The candidate became a survivor after the recheck under row lock.
    NowSurvivor,
    /// The candidate row was removed by a concurrent sweep before this one
    /// could lock it.
    RowRemovedConcurrently,
}

/// Bounded outcome of a single retention sweep, shared by dry-run and delete
/// modes. Carries only bounded counts and fixed classes — never
/// project/generation/commit/hash identity labels — so it is safe for telemetry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionSweepOutcome {
    /// Operating mode that produced this outcome.
    pub mode: RetentionMode,
    /// How many non-survivor generations were identified as candidates in this
    /// sweep (bounded by [`MAX_RETENTION_BATCH`]).
    pub candidates: usize,
    /// How many generations were actually deleted (0 in dry-run).
    pub deleted: usize,
    /// Total generations retained as survivors after recompute.
    pub survivors: usize,
    /// Bounded skip count by fixed class.
    pub skipped_active_pin: usize,
    pub skipped_now_survivor: usize,
    pub skipped_removed_concurrently: usize,
    /// How many serialization/deadlock retries were performed (defense in depth).
    pub retries: usize,
}

impl RetentionSweepOutcome {
    /// Total skipped candidates across all fixed classes.
    pub fn total_skipped(&self) -> usize {
        self.skipped_active_pin + self.skipped_now_survivor + self.skipped_removed_concurrently
    }
}

/// The production graph-retention repository.
pub struct RepoGraphRetentionRepository {
    db: Database,
}

impl RepoGraphRetentionRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Run one bounded retention sweep for a project.
    ///
    /// In `dry_run` mode this recomputes survivors and selects candidates using
    /// the same logic as `delete` but performs no deletes. In `delete` mode it
    /// deletes at most [`MAX_RETENTION_BATCH`] non-survivor generations.
    /// Serialization/deadlock errors are retried as defense in depth.
    pub async fn sweep(
        &self,
        request: RetentionSweepRequest<'_>,
    ) -> DbResult<RetentionSweepOutcome> {
        self.sweep_with_retry(request, DEFAULT_MAX_TX_RETRIES).await
    }

    /// Run a sweep with the caller's already-validated bounded retry budget.
    pub async fn sweep_with_retry(
        &self,
        request: RetentionSweepRequest<'_>,
        max_retries: usize,
    ) -> DbResult<RetentionSweepOutcome> {
        let history_n = request.history_n.max(MIN_RETENTION_HISTORY_N);
        if request.mode == RetentionMode::Off {
            return Ok(RetentionSweepOutcome {
                mode: RetentionMode::Off,
                candidates: 0,
                deleted: 0,
                survivors: 0,
                skipped_active_pin: 0,
                skipped_now_survivor: 0,
                skipped_removed_concurrently: 0,
                retries: 0,
            });
        }

        let project_id = request.project_id.to_owned();
        let mode = request.mode;
        // Track attempt count so the outcome reports bounded retries. The
        // atomic is safe because each attempt is awaited to completion before
        // the next one starts (the retry helper is sequential).
        let attempt = std::sync::atomic::AtomicUsize::new(0);
        let outcome = retry_on_serialization_failure(max_retries, || {
            let project_id = project_id.clone();
            let current_attempt = attempt.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                let mut outcome = self.sweep_once(&project_id, mode, history_n).await?;
                // Report how many retries preceded the successful attempt.
                outcome.retries = current_attempt;
                Ok(outcome)
            }
        })
        .await?;
        Ok(outcome)
    }

    /// Execute a single sweep attempt.
    async fn sweep_once(
        &self,
        project_id: &str,
        mode: RetentionMode,
        history_n: usize,
    ) -> DbResult<RetentionSweepOutcome> {
        self.db.ensure_initialized().await?;

        // Acquire a dedicated connection so we fully control its lifecycle.
        // Session advisory locks survive transaction commit/rollback, so if any
        // pin is left unreleased we must discard the connection rather than
        // return it to the pool.
        let mut conn = self.db.pool().acquire().await?;
        let mut tx = conn.begin().await?;

        // Acquire the same project publication advisory lock used by migration
        // 125. Transaction-scoped: released on commit/rollback.
        acquire_project_publication_lock(&mut tx, project_id).await?;

        // Lock and re-read the current pointer under the project advisory lock.
        let current_generation_id = lock_and_read_current_generation(&mut tx, project_id).await?;

        // Recompute the survivor set: current generation union newest N.
        let survivor_ids = recompute_survivor_set(
            &mut tx,
            project_id,
            history_n,
            current_generation_id.as_deref(),
        )
        .await?;
        let total_survivors = survivor_ids.len();

        // Process candidates in a single unified pass that preserves the lock
        // order (project advisory → current → candidate row → stream pin),
        // pages past actively-pinned rows, and fills a batch of up to
        // MAX_RETENTION_BATCH actual candidates.
        let result = process_candidates(&mut tx, project_id, &survivor_ids, mode).await;

        match result {
            Ok(processing) => {
                // Release all session pins still held (delete mode) before
                // commit. Dry-run releases each pin inline during processing.
                for key in &processing.pinned_keys {
                    match release_generation_stream_pin_exclusive(&mut tx, *key).await {
                        Ok(true) => { /* released cleanly */ }
                        Ok(false) => {
                            // The session did not hold this exclusive lock.
                            // Discard the connection: rollback does not release
                            // session advisory locks.
                            drop(tx);
                            conn.close_on_drop();
                            return Err(DbError::InvalidData(format!(
                                "retention exclusive stream pin class={} object={} \
                                 was not held on release",
                                key.class_id, key.object_id
                            )));
                        }
                        Err(error) => {
                            // The unlock query itself failed (e.g. cancellation
                            // or timeout). The session may still hold the lock.
                            drop(tx);
                            conn.close_on_drop();
                            return Err(DbError::Sqlx(error));
                        }
                    }
                }

                if mode == RetentionMode::DryRun {
                    tx.rollback().await?;
                } else {
                    tx.commit().await?;
                }

                Ok(RetentionSweepOutcome {
                    mode,
                    candidates: processing.candidates,
                    deleted: processing.deleted,
                    survivors: total_survivors,
                    skipped_active_pin: processing.skipped_active_pin,
                    skipped_now_survivor: processing.skipped_now_survivor,
                    skipped_removed_concurrently: processing.skipped_removed_concurrently,
                    retries: 0,
                })
            }
            Err(error) => {
                // Any error during candidate processing (including dry-run pin
                // release failures): discard the connection so PostgreSQL
                // releases all session advisory locks when the backend closes.
                // Transaction rollback does not release session advisory locks,
                // so discarding is the only safe path. Correctness does not
                // depend on a deadlock victim because the nonblocking pin never
                // waits.
                drop(tx);
                conn.close_on_drop();
                Err(error)
            }
        }
    }
}

/// Acquire the same project publication advisory lock used by migration 125:
/// `pg_advisory_xact_lock(hashtextextended(project_id, 0))`.
///
/// Transaction-scoped: released automatically on commit/rollback.
async fn acquire_project_publication_lock(
    tx: &mut Transaction<'_, Postgres>,
    project_id: &str,
) -> DbResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(project_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Lock the current pointer row (`FOR UPDATE`) and read its generation id.
/// Locking/re-reading `repo_graph_current` under the project advisory lock
/// ensures the survivor set reflects the latest publication.
async fn lock_and_read_current_generation(
    tx: &mut Transaction<'_, Postgres>,
    project_id: &str,
) -> DbResult<Option<String>> {
    Ok(sqlx::query_scalar(
        "SELECT generation_id::text FROM repo_graph_current \
             WHERE project_id = $1 FOR UPDATE",
    )
    .bind(project_id)
    .fetch_optional(&mut **tx)
    .await?)
}

/// Recompute survivors: the current generation union the newest `history_n`
/// generations by `publish_seq DESC`. Handles current-outside-newest-N and
/// same-commit rotations because membership is by generation id, not commit.
///
/// `locked_current` is the generation id read under the `FOR UPDATE` row lock
/// on `repo_graph_current`. Passing it in avoids a second unlocked read.
async fn recompute_survivor_set(
    tx: &mut Transaction<'_, Postgres>,
    project_id: &str,
    history_n: usize,
    locked_current: Option<&str>,
) -> DbResult<Vec<String>> {
    // Newest N publish_seq generations (DESC) — deterministic because
    // publish_seq is a UNIQUE IDENTITY column.
    let newest: Vec<String> = sqlx::query_scalar(
        "SELECT generation_id::text FROM repo_graph_generation \
         WHERE project_id = $1 \
         ORDER BY publish_seq DESC \
         LIMIT $2",
    )
    .bind(project_id)
    .bind(history_n as i64)
    .fetch_all(&mut **tx)
    .await?;

    // Union, preserving deterministic order (newest first, then current tail).
    let mut survivors: Vec<String> = Vec::with_capacity(newest.len() + 1);
    for id in &newest {
        if !survivors.contains(id) {
            survivors.push(id.clone());
        }
    }
    if let Some(current_id) = locked_current
        && !survivors.iter().any(|id| id == current_id)
    {
        survivors.push(current_id.to_owned());
    }
    Ok(survivors)
}

/// Bounded result of processing non-survivor candidates in one sweep.
#[derive(Clone, Debug, Default)]
struct CandidateProcessingResult {
    /// How many non-survivor generations were confirmed as actual candidates
    /// (bounded by [`MAX_RETENTION_BATCH`]).
    candidates: usize,
    /// How many generations were actually deleted (0 in dry-run).
    deleted: usize,
    /// Bounded skip counters by fixed class.
    skipped_active_pin: usize,
    skipped_now_survivor: usize,
    skipped_removed_concurrently: usize,
    /// Session-scoped exclusive pins still held (delete mode only). The caller
    /// must release each before commit, or discard the connection on error.
    pinned_keys: Vec<GenerationStreamPinKey>,
}

/// Unified candidate processing: pages through non-survivor generations in
/// deterministic oldest-`publish_seq`-first order, and for each one:
///
/// 1. **Lock the candidate row** (`FOR UPDATE`) — this is the candidate-row
///    lock in the required order.
/// 2. **Recheck candidacy** against the recomputed survivor set (defense in
///    depth; the project advisory lock already prevents concurrent
///    publication from advancing current).
/// 3. **Take the canonical nonblocking exclusive session pin** without waiting
///    — this is acquired *after* the row lock, preserving
///    `candidate row → generation stream pin`.
/// 4. In `dry_run`: release the pin immediately (no delete). In `delete`:
///    delete the compatibility row before the immutable generation and rely on
///    FK cascades; hold the pin until the caller releases it before commit.
///
/// The scan **continues past actively-pinned rows** via keyset pagination
/// until [`MAX_RETENTION_BATCH`] actual candidates are found or the
/// non-survivor set is exhausted. This guarantees a batch fills from available
/// (unpinned) candidates even when older non-survivors are pinned.
///
/// **Error safety:** if any post-acquisition error or unlock-integrity failure
/// occurs, the function returns `Err`. The caller discards the connection so
/// PostgreSQL releases all session advisory locks when the backend closes.
async fn process_candidates(
    tx: &mut Transaction<'_, Postgres>,
    project_id: &str,
    survivor_ids: &[String],
    mode: RetentionMode,
) -> DbResult<CandidateProcessingResult> {
    let survivor_set: std::collections::HashSet<&String> = survivor_ids.iter().collect();
    let mut result = CandidateProcessingResult::default();
    // Keyset cursor: start before the first possible publish_seq.
    let mut last_publish_seq: i64 = 0;

    loop {
        if result.candidates >= MAX_RETENTION_BATCH {
            break;
        }

        // Keyset page: fetch the next batch of generation IDs older than the
        // cursor, in deterministic publish_seq ASC order. We fetch all
        // generations (including survivors) and filter in Rust because the
        // survivor set is computed in application code.
        let page: Vec<(String, i64)> = sqlx::query_as(
            "SELECT generation_id::text, publish_seq FROM repo_graph_generation \
             WHERE project_id = $1 AND publish_seq > $2 \
             ORDER BY publish_seq ASC \
             LIMIT $3",
        )
        .bind(project_id)
        .bind(last_publish_seq)
        .bind(RETENTION_SCAN_PAGE_SIZE)
        .fetch_all(&mut **tx)
        .await?;

        if page.is_empty() {
            break; // all generations exhausted
        }

        for (generation_id, publish_seq) in page {
            if result.candidates >= MAX_RETENTION_BATCH {
                break;
            }
            last_publish_seq = publish_seq;

            // Skip survivors — they are never candidates.
            if survivor_set.contains(&generation_id) {
                continue;
            }

            // ── Lock the candidate row (FOR UPDATE) ──
            // This is the candidate-row lock in the required order:
            // project advisory → current → candidate row → stream pin.
            let still_exists: Option<(String,)> = sqlx::query_as(
                "SELECT generation_id::text FROM repo_graph_generation \
                 WHERE generation_id = $1::uuid \
                 FOR UPDATE",
            )
            .bind(&generation_id)
            .fetch_optional(&mut **tx)
            .await?;
            if still_exists.is_none() {
                // Removed by a concurrent sweep before we could lock it.
                result.skipped_removed_concurrently += 1;
                continue;
            }

            // ── Recheck candidacy under the row lock ──
            // The project advisory lock prevents a concurrent publication from
            // advancing current, but this recheck is defense in depth.
            let is_current: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM repo_graph_current WHERE generation_id = $1::uuid)",
            )
            .bind(&generation_id)
            .fetch_one(&mut **tx)
            .await?;
            if is_current || survivor_set.contains(&generation_id) {
                result.skipped_now_survivor += 1;
                continue;
            }

            // ── Take the canonical nonblocking exclusive session pin ──
            // Acquired AFTER the row lock, preserving lock order. Uses
            // pg_try_advisory_lock so actively-pinned rows are skipped without
            // waiting — correctness never depends on a deadlock victim.
            let key = match repo_graph_generation::generation_stream_pin_key(&generation_id) {
                Ok(key) => key,
                Err(_) => {
                    // A malformed id should not exist in the immutable table;
                    // skip rather than abort the whole sweep.
                    result.skipped_now_survivor += 1;
                    continue;
                }
            };
            let acquired = try_acquire_generation_stream_pin_exclusive(&mut *tx, key).await?;
            if !acquired {
                // A reader holds the shared pin: skip without waiting and
                // continue scanning for the next available candidate.
                result.skipped_active_pin += 1;
                continue;
            }

            // ── Actual candidate confirmed ──
            result.candidates += 1;

            if mode == RetentionMode::DryRun {
                // Dry-run: release the pin immediately (no delete). If the
                // release fails, return Err so the caller discards the
                // connection — it may still hold the session advisory lock
                // because transaction rollback does not release session locks.
                match release_generation_stream_pin_exclusive(&mut *tx, key).await {
                    Ok(true) => { /* released cleanly */ }
                    Ok(false) => {
                        return Err(DbError::InvalidData(format!(
                            "retention dry-run exclusive stream pin class={} object={} \
                             was not held on release",
                            key.class_id, key.object_id
                        )));
                    }
                    Err(error) => {
                        return Err(DbError::Sqlx(error));
                    }
                }
            } else {
                // Delete mode: hold the pin until the caller releases it
                // before commit.
                result.pinned_keys.push(key);

                // Delete the compatibility row before the immutable generation.
                // The FK repo_graph_cache -> repo_graph_generation is DEFERRABLE
                // INITIALLY DEFERRED; deleting the cache row first avoids
                // relying on the cascade for it and preserves the
                // compatibility-row-before-generation ordering required by the
                // lock order contract.
                sqlx::query("DELETE FROM repo_graph_cache WHERE generation_id = $1::uuid")
                    .bind(&generation_id)
                    .execute(&mut **tx)
                    .await?;

                // Delete the immutable generation; FK cascades remove
                // repo_graph_galaxy_artifact, repo_graph_galaxy_chunk, and
                // repo_graph_current (if it pointed here — but we just
                // rechecked that it does not).
                sqlx::query("DELETE FROM repo_graph_generation WHERE generation_id = $1::uuid")
                    .bind(&generation_id)
                    .execute(&mut **tx)
                    .await?;
                result.deleted += 1;
            }
        }
    }

    Ok(result)
}

#[cfg(test)]
#[path = "repo_graph_retention_tests.rs"]
mod tests;
