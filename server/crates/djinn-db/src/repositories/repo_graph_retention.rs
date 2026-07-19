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
//! current pointer → candidate → generation stream pin.
//!
//! Bounded deadlock/serialization retry ([`retry_on_serialization_failure`])
//! is applied as defense in depth; correctness does not depend on choosing a
//! deadlock victim because the lock order is consistent and the stream pin is
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

/// Bounded scan limit: examine at most this many non-survivor rows while
/// filling a batch, so a huge backlog cannot cause an unbounded read even
/// though only [`MAX_RETENTION_BATCH`] rows are ever deleted.
const RETENTION_SCAN_LIMIT: usize = 512;

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
        let outcome = retry_on_serialization_failure(DEFAULT_MAX_TX_RETRIES, || {
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

        // Enumerate non-survivors deterministically, scanning past actively
        // pinned rows, up to MAX_RETENTION_BATCH actual candidates.
        let scan = enumerate_non_survivors(&mut tx, project_id, &survivor_ids).await?;
        let candidates = scan.candidates;
        let total_survivors = survivor_ids.len();

        if mode == RetentionMode::DryRun {
            // Dry-run uses the same selection/recheck logic but performs no
            // deletes.
            tx.rollback().await?;
            return Ok(RetentionSweepOutcome {
                mode,
                candidates,
                deleted: 0,
                survivors: total_survivors,
                skipped_active_pin: scan.skipped_active_pin,
                skipped_now_survivor: scan.skipped_now_survivor,
                skipped_removed_concurrently: scan.skipped_removed_concurrently,
                retries: 0,
            });
        }

        // Delete mode: for each candidate, recheck candidacy under row lock,
        // take the nonblocking exclusive session pin, delete compatibility row
        // before the immutable generation, and let FK cascades remove the rest.
        let delete_result = delete_candidates(&mut tx, &scan.candidate_ids, &survivor_ids).await;

        match delete_result {
            Ok((deleted, pinned_keys)) => {
                // Release all session pins before commit. If any unlock returns
                // false or errors, discard the connection to avoid leaking
                // session advisory locks into the pool.
                for key in &pinned_keys {
                    match release_generation_stream_pin_exclusive(&mut tx, *key).await {
                        Ok(true) => { /* released cleanly */ }
                        Ok(false) => {
                            drop(tx);
                            conn.close_on_drop();
                            return Err(DbError::InvalidData(format!(
                                "retention exclusive stream pin class={} object={} \
                                 was not held on release",
                                key.class_id, key.object_id
                            )));
                        }
                        Err(error) => {
                            drop(tx);
                            conn.close_on_drop();
                            return Err(DbError::Sqlx(error));
                        }
                    }
                }
                tx.commit().await?;
                Ok(RetentionSweepOutcome {
                    mode,
                    candidates,
                    deleted,
                    survivors: total_survivors,
                    skipped_active_pin: scan.skipped_active_pin,
                    skipped_now_survivor: scan.skipped_now_survivor,
                    skipped_removed_concurrently: scan.skipped_removed_concurrently,
                    retries: 0,
                })
            }
            Err(error) => {
                // Error after potentially acquiring some session pins: rollback
                // the transaction and discard the connection so PostgreSQL
                // releases all session advisory locks when the backend closes.
                // Correctness does not depend on a deadlock victim because the
                // nonblocking pin never waits.
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

/// Deterministically enumerated non-survivor scan result. Candidates are the
/// actual deletion targets (bounded to [`MAX_RETENTION_BATCH`]); the skip
/// counters reflect rows scanned past while filling the batch.
#[derive(Clone, Debug, Default)]
struct NonSurvivorScan {
    /// Deterministic, oldest-publish_seq-first candidate ids.
    candidate_ids: Vec<String>,
    candidates: usize,
    skipped_active_pin: usize,
    skipped_now_survivor: usize,
    skipped_removed_concurrently: usize,
}

/// Enumerate non-survivors in deterministic order (oldest `publish_seq` first),
/// continuing past rows whose stream pin is actively held so a batch can fill
/// up to [`MAX_RETENTION_BATCH`] actual candidates.
///
/// The pinned check is nonblocking: [`try_acquire_generation_stream_pin_exclusive`]
/// uses `pg_try_advisory_lock`, which returns immediately. If it succeeds, no
/// reader holds the shared pin; we release it immediately so the delete loop
/// can re-acquire under the row lock. If it fails, the row is skipped (counted)
/// and the scan continues to the next candidate.
async fn enumerate_non_survivors(
    tx: &mut Transaction<'_, Postgres>,
    project_id: &str,
    survivor_ids: &[String],
) -> DbResult<NonSurvivorScan> {
    // Select non-survivor generation ids in deterministic oldest-first order.
    // Fetch up to RETENTION_SCAN_LIMIT rows so the scan is bounded.
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT generation_id::text FROM repo_graph_generation \
         WHERE project_id = $1 \
         ORDER BY publish_seq ASC \
         LIMIT $2",
    )
    .bind(project_id)
    .bind(RETENTION_SCAN_LIMIT as i64)
    .fetch_all(&mut **tx)
    .await?;

    let survivor_set: std::collections::HashSet<&String> = survivor_ids.iter().collect();
    let mut scan = NonSurvivorScan::default();

    for (generation_id,) in rows {
        if scan.candidate_ids.len() >= MAX_RETENTION_BATCH {
            break;
        }
        // Survivor rows are never candidates.
        if survivor_set.contains(&generation_id) {
            continue;
        }

        // Nonblocking probe of the stream pin using the canonical key. If a
        // reader holds the shared pin, `pg_try_advisory_lock` returns false.
        let key = match repo_graph_generation::generation_stream_pin_key(&generation_id) {
            Ok(key) => key,
            Err(_) => {
                // A malformed id should not exist in the immutable table;
                // skip rather than abort the whole sweep.
                scan.skipped_now_survivor += 1;
                continue;
            }
        };
        let acquired = try_acquire_generation_stream_pin_exclusive(&mut *tx, key).await?;
        if acquired {
            // No active reader: release immediately; the delete loop will
            // re-acquire under the row lock.
            if !release_generation_stream_pin_exclusive(&mut *tx, key).await? {
                return Err(DbError::InvalidData(
                    "retention scan stream pin was not held on release".to_owned(),
                ));
            }
            scan.candidate_ids.push(generation_id);
            scan.candidates += 1;
        } else {
            // Actively pinned by a reader: skip without waiting, keep filling.
            scan.skipped_active_pin += 1;
        }
    }

    Ok(scan)
}

/// Delete each candidate: lock + recheck candidacy under the row lock, take the
/// canonical nonblocking exclusive session pin (without waiting), delete the
/// compatibility row before the immutable generation, and let FK cascades
/// remove artifact/chunks. Returns the count actually deleted plus all pinned
/// keys (session-scoped, must be released by the caller).
async fn delete_candidates(
    tx: &mut Transaction<'_, Postgres>,
    candidate_ids: &[String],
    survivor_ids: &[String],
) -> DbResult<(usize, Vec<GenerationStreamPinKey>)> {
    let survivor_set: std::collections::HashSet<&String> = survivor_ids.iter().collect();
    let mut deleted = 0usize;
    let mut pinned_keys: Vec<GenerationStreamPinKey> = Vec::new();

    for generation_id in candidate_ids {
        if deleted >= MAX_RETENTION_BATCH {
            break;
        }
        // Lock the immutable generation row (`FOR UPDATE`) and recheck that it
        // still exists. If removed by a concurrent sweep, skip it.
        let still_exists: Option<(String,)> = sqlx::query_as(
            "SELECT generation_id::text FROM repo_graph_generation \
             WHERE generation_id = $1::uuid \
             FOR UPDATE",
        )
        .bind(generation_id)
        .fetch_optional(&mut **tx)
        .await?;
        if still_exists.is_none() {
            continue;
        }

        // Recheck candidacy: is this generation now the current pointer or in
        // the recomputed survivor set? The project advisory lock prevents a
        // concurrent publication from advancing current, but this recheck is
        // defense in depth.
        let is_current: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM repo_graph_current WHERE generation_id = $1::uuid)",
        )
        .bind(generation_id)
        .fetch_one(&mut **tx)
        .await?;
        if is_current || survivor_set.contains(generation_id) {
            continue;
        }

        // Take the canonical nonblocking exclusive session pin without waiting.
        let key = repo_graph_generation::generation_stream_pin_key(generation_id)?;
        let acquired = try_acquire_generation_stream_pin_exclusive(&mut *tx, key).await?;
        if !acquired {
            // A reader pinned this generation between scan and lock.
            // Skip without waiting; correctness never depends on a victim.
            continue;
        }
        pinned_keys.push(key);

        // Delete the compatibility row before the immutable generation. The FK
        // repo_graph_cache -> repo_graph_generation is DEFERRABLE INITIALLY
        // DEFERRED; deleting the cache row first avoids relying on the cascade
        // for it and preserves the compatibility-row-before-generation ordering
        // required by the lock order contract.
        sqlx::query("DELETE FROM repo_graph_cache WHERE generation_id = $1::uuid")
            .bind(generation_id)
            .execute(&mut **tx)
            .await?;

        // Delete the immutable generation; FK cascades remove
        // repo_graph_galaxy_artifact, repo_graph_galaxy_chunk, and
        // repo_graph_current (if it pointed here — but we just rechecked that
        // it does not).
        sqlx::query("DELETE FROM repo_graph_generation WHERE generation_id = $1::uuid")
            .bind(generation_id)
            .execute(&mut **tx)
            .await?;
        deleted += 1;
    }

    Ok((deleted, pinned_keys))
}

#[cfg(test)]
#[path = "repo_graph_retention_tests.rs"]
mod tests;
