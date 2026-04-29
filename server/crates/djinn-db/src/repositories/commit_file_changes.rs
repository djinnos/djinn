//! Raw per-commit per-file change facts harvested from `git log`.
//!
//! The warmer pipeline (`djinn_graph::coupling_index::ingest_new_commits`)
//! batches rows into this table; every aggregate the MCP `code_graph`
//! tool exposes (`coupling`, `churn`, …) is computed at query time, so
//! policy knobs like "skip commits that touch N+ files" or "weight by
//! recency" are parameters on the read path instead of schema
//! migrations.
//!
//! Modelled after [`crate::repositories::repo_graph_cache::RepoGraphCacheRepository`]
//! — see that file for the rationale behind Dolt / VARCHAR timestamp /
//! batched upsert patterns.

use crate::Result;
use crate::database::Database;

/// One row in `commit_file_changes`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct CommitFileChange {
    pub project_id: String,
    pub commit_sha: String,
    pub file_path: String,
    /// Git `--name-status` change kind: `A`, `M`, `D`, `T`, or
    /// `R<score>` / `C<score>` for renames/copies.
    pub change_kind: String,
    /// ISO-8601 UTC timestamp (matches the rest of the schema; see
    /// migration 2 for the rationale behind VARCHAR-as-timestamp).
    pub committed_at: String,
    pub author_email: String,
    pub insertions: i64,
    pub deletions: i64,
    pub old_path: Option<String>,
}

/// Coupled-file row emitted by
/// [`CommitFileChangeRepository::top_coupled`].
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct CoupledFile {
    pub file_path: String,
    pub co_edit_count: i64,
    pub last_co_edit: String,
    /// Up to three sample SHAs from the supporting commits, newest-first.
    /// Emitted as a comma-separated string by the underlying SQL and
    /// split into a `Vec` by [`CommitFileChangeRepository::top_coupled`].
    #[sqlx(rename = "supporting_commit_samples")]
    pub supporting_commit_samples_raw: String,
}

impl CoupledFile {
    pub fn supporting_commit_samples(&self) -> Vec<String> {
        self.supporting_commit_samples_raw
            .split(',')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_owned())
            .collect()
    }
}

/// Churn row emitted by [`CommitFileChangeRepository::churn`].
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct FileChurn {
    pub file_path: String,
    pub commit_count: i64,
    pub insertions: i64,
    pub deletions: i64,
    pub last_commit_at: String,
}

/// One file pair emitted by
/// [`CommitFileChangeRepository::top_coupled_pairs`]. Canonical
/// ordering: `file_a < file_b` lexicographically so each unordered
/// pair appears exactly once in the result set.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct CoupledPair {
    pub file_a: String,
    pub file_b: String,
    pub co_edits: i64,
    pub last_co_edit: String,
}

/// One file-level hub emitted by
/// [`CommitFileChangeRepository::coupling_hubs`]. Scored by cumulative
/// coupling across all partners — a high total_coupling with a low
/// partner_count flags "always co-edited with the same small cluster",
/// while a high value on both flags a change-propagation hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CouplingHub {
    pub file_path: String,
    pub total_coupling: i64,
    pub partner_count: i64,
}

/// One per-commit pair fact stored in `coupling_pair_events`.
/// `file_a < file_b` is enforced at insert time so each unordered pair
/// appears at most once per commit.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct CouplingPairEvent {
    pub project_id: String,
    pub file_a: String,
    pub file_b: String,
    pub commit_sha: String,
    pub committed_at: String,
}

/// Maximum files-per-commit to consider for pair generation. Commits
/// touching more files than this (lockfile refreshes, codemods,
/// initial-import "fat" commits) generate near-quadratic pair counts
/// with little real coupling signal — they get dropped at write time
/// instead of filtered at read time. Matches the historical
/// `max_files_per_commit` default of 15 used by the MCP dispatch.
pub const MAX_FILES_PER_COMMIT_FOR_PAIRS: usize = 15;

pub struct CommitFileChangeRepository {
    db: Database,
}

impl CommitFileChangeRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Batch upsert. Idempotent on the `(project_id, commit_sha,
    /// file_path)` composite PK — callers can re-run on overlapping
    /// ranges (e.g. after a partial ingest failure).
    pub async fn upsert_batch(&self, rows: &[CommitFileChange]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        self.db.ensure_initialized().await?;

        // Build the multi-row INSERT statement. Dolt caps prepared-statement
        // bind parameters at ~65k, and each row here uses 9 params, so we
        // stay well under the cap with the 500-row batch the ingest layer
        // feeds us (≈4.5k params).
        let mut sql = String::from(
            "INSERT INTO commit_file_changes \
             (project_id, commit_sha, file_path, change_kind, committed_at, \
              author_email, insertions, deletions, old_path) VALUES ",
        );
        for i in 0..rows.len() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str("(?, ?, ?, ?, ?, ?, ?, ?, ?)");
        }
        sql.push_str(
            " ON DUPLICATE KEY UPDATE \
               change_kind = VALUES(change_kind), \
               committed_at = VALUES(committed_at), \
               author_email = VALUES(author_email), \
               insertions = VALUES(insertions), \
               deletions = VALUES(deletions), \
               old_path = VALUES(old_path)",
        );

        let mut query = sqlx::query(&sql);
        for row in rows {
            query = query
                .bind(&row.project_id)
                .bind(&row.commit_sha)
                .bind(&row.file_path)
                .bind(&row.change_kind)
                .bind(&row.committed_at)
                .bind(&row.author_email)
                .bind(row.insertions)
                .bind(row.deletions)
                .bind(row.old_path.as_deref());
        }
        query.execute(self.db.pool()).await?;
        Ok(rows.len())
    }

    /// Batch upsert of pair events into `coupling_pair_events`.
    /// Idempotent on the `(project_id, file_a, file_b, commit_sha)` PK
    /// — replays no-op. Callers should derive pairs via
    /// [`derive_pair_events`] (skips fat commits, enforces
    /// `file_a < file_b`).
    pub async fn upsert_pair_events(&self, events: &[CouplingPairEvent]) -> Result<usize> {
        if events.is_empty() {
            return Ok(0);
        }
        self.db.ensure_initialized().await?;

        // Dolt prepared-statement parameters cap is ~65k; 5 params per
        // row leaves us well under the cap at 500-row batches.
        let mut sql = String::from(
            "INSERT INTO coupling_pair_events \
             (project_id, file_a, file_b, commit_sha, committed_at) VALUES ",
        );
        for i in 0..events.len() {
            if i > 0 {
                sql.push(',');
            }
            sql.push_str("(?, ?, ?, ?, ?)");
        }
        // Replays must be no-ops. Touch any non-PK column with the
        // same value so the UPSERT contract is satisfied without
        // changing semantics on duplicate keys.
        sql.push_str(
            " ON DUPLICATE KEY UPDATE committed_at = VALUES(committed_at)",
        );

        let mut query = sqlx::query(&sql);
        for e in events {
            query = query
                .bind(&e.project_id)
                .bind(&e.file_a)
                .bind(&e.file_b)
                .bind(&e.commit_sha)
                .bind(&e.committed_at);
        }
        query.execute(self.db.pool()).await?;
        Ok(events.len())
    }

    /// Count rows in `coupling_pair_events` for a project. Cheap —
    /// hits the PK prefix `(project_id, ...)`. Used by the ingest path
    /// to detect "first run after migration 20 landed" and trigger a
    /// one-shot backfill from `commit_file_changes`.
    pub async fn pair_events_count_for_project(&self, project_id: &str) -> Result<usize> {
        self.db.ensure_initialized().await?;
        use sqlx::Row;
        let row = sqlx::query(
            "SELECT COUNT(*) AS row_count \
             FROM coupling_pair_events WHERE project_id = ?",
        )
        .bind(project_id)
        .fetch_one(self.db.pool())
        .await?;
        let count: i64 = row.get("row_count");
        Ok(count.max(0) as usize)
    }

    /// Backfill helper: rebuild `coupling_pair_events` for `project_id`
    /// from the existing rows in `commit_file_changes`. Used the first
    /// time the new schema runs against a project that was ingested
    /// before pair materialisation existed.
    ///
    /// Streams rows by commit so we never hold the whole project in
    /// memory. Idempotent — re-running just upserts the same events.
    pub async fn rebuild_pair_events_for_project(&self, project_id: &str) -> Result<usize> {
        self.db.ensure_initialized().await?;
        use sqlx::Row;

        // Pull rows ordered by (commit_sha, file_path) so we can group
        // a single sweep without an in-memory hash. Per-commit work is
        // bounded by MAX_FILES_PER_COMMIT_FOR_PAIRS²/2 = 105 pair
        // candidates; commits that exceed the cap are dropped.
        let rows = sqlx::query(
            "SELECT commit_sha, file_path, committed_at \
             FROM commit_file_changes \
             WHERE project_id = ? \
             ORDER BY commit_sha ASC, file_path ASC",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        let mut written = 0usize;
        let mut current_sha: Option<String> = None;
        let mut current_committed_at: Option<String> = None;
        let mut files: Vec<String> = Vec::new();
        const FLUSH_AT: usize = 500;
        let mut buffer: Vec<CouplingPairEvent> = Vec::with_capacity(FLUSH_AT);

        for row in rows {
            let sha: String = row.get("commit_sha");
            let file: String = row.get("file_path");
            let committed_at: String = row.get("committed_at");

            if Some(&sha) != current_sha.as_ref() {
                if let (Some(prev_sha), Some(prev_at)) =
                    (current_sha.take(), current_committed_at.take())
                {
                    derive_pair_events_into(
                        project_id,
                        &prev_sha,
                        &prev_at,
                        &files,
                        &mut buffer,
                    );
                    if buffer.len() >= FLUSH_AT {
                        written += self.upsert_pair_events(&buffer).await?;
                        buffer.clear();
                    }
                }
                current_sha = Some(sha);
                current_committed_at = Some(committed_at);
                files.clear();
            }
            files.push(file);
        }
        if let (Some(sha), Some(at)) = (current_sha, current_committed_at) {
            derive_pair_events_into(project_id, &sha, &at, &files, &mut buffer);
        }
        if !buffer.is_empty() {
            written += self.upsert_pair_events(&buffer).await?;
        }
        Ok(written)
    }

    /// Read the per-project ingest cursor (last SHA walked).
    pub async fn get_cursor(&self, project_id: &str) -> Result<Option<String>> {
        self.db.ensure_initialized().await?;
        use sqlx::Row;
        Ok(sqlx::query(
            "SELECT last_indexed_sha FROM coupling_cursor WHERE project_id = ?",
        )
        .bind(project_id)
        .fetch_optional(self.db.pool())
        .await?
        .map(|row| row.get("last_indexed_sha")))
    }

    /// Advance the per-project cursor to `sha`. Idempotent.
    pub async fn set_cursor(&self, project_id: &str, sha: &str) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query(
            "INSERT INTO coupling_cursor (project_id, last_indexed_sha, last_updated_at) \
             VALUES (?, ?, DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ')) \
             ON DUPLICATE KEY UPDATE \
               last_indexed_sha = VALUES(last_indexed_sha), \
               last_updated_at = VALUES(last_updated_at)",
        )
        .bind(project_id)
        .bind(sha)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    /// Files most frequently co-edited with `file_path`, by distinct
    /// commit count. Limit is capped by the caller.
    ///
    /// Reads from the materialised `coupling_pair_events` table (one
    /// row per pair-per-commit). The previous self-join over
    /// `commit_file_changes` was retired because Dolt's planner does
    /// not push the big-commit cap subquery early — see migration 20.
    pub async fn top_coupled(
        &self,
        project_id: &str,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CoupledFile>> {
        self.db.ensure_initialized().await?;
        // The pair table stores `file_a < file_b`, so the seed appears
        // on either side. UNION ALL the two halves, project the
        // counterpart as `file_path`, then aggregate.
        let limit = limit.clamp(1, 500) as i64;
        let rows: Vec<CoupledFile> = sqlx::query_as(
            "SELECT \
                file_path, \
                CAST(COUNT(DISTINCT commit_sha) AS SIGNED) AS co_edit_count, \
                MAX(committed_at) AS last_co_edit, \
                GROUP_CONCAT(DISTINCT commit_sha ORDER BY committed_at DESC SEPARATOR ',') AS supporting_commit_samples \
             FROM ( \
                 SELECT file_b AS file_path, commit_sha, committed_at \
                   FROM coupling_pair_events \
                  WHERE project_id = ? AND file_a = ? \
                 UNION ALL \
                 SELECT file_a AS file_path, commit_sha, committed_at \
                   FROM coupling_pair_events \
                  WHERE project_id = ? AND file_b = ? \
             ) AS peers \
             GROUP BY file_path \
             ORDER BY co_edit_count DESC, last_co_edit DESC, file_path ASC \
             LIMIT ?",
        )
        .bind(project_id)
        .bind(file_path)
        .bind(project_id)
        .bind(file_path)
        .bind(limit)
        .fetch_all(self.db.pool())
        .await?;
        Ok(rows)
    }

    /// Top `limit` files by distinct commit count within the optional
    /// time window. `since` is an ISO-8601 UTC lower bound; when
    /// `None`, no time filter is applied.
    pub async fn churn(
        &self,
        project_id: &str,
        limit: usize,
        since: Option<&str>,
    ) -> Result<Vec<FileChurn>> {
        self.db.ensure_initialized().await?;
        let limit = limit.clamp(1, 500) as i64;
        let rows: Vec<FileChurn> = match since {
            Some(ts) => sqlx::query_as(
                "SELECT \
                    file_path, \
                    CAST(COUNT(DISTINCT commit_sha) AS SIGNED) AS commit_count, \
                    CAST(COALESCE(SUM(insertions), 0) AS SIGNED) AS insertions, \
                    CAST(COALESCE(SUM(deletions), 0) AS SIGNED) AS deletions, \
                    MAX(committed_at) AS last_commit_at \
                 FROM commit_file_changes \
                 WHERE project_id = ? AND committed_at >= ? \
                 GROUP BY file_path \
                 ORDER BY commit_count DESC, last_commit_at DESC, file_path ASC \
                 LIMIT ?",
            )
            .bind(project_id)
            .bind(ts)
            .bind(limit)
            .fetch_all(self.db.pool())
            .await?,
            None => sqlx::query_as(
                "SELECT \
                    file_path, \
                    CAST(COUNT(DISTINCT commit_sha) AS SIGNED) AS commit_count, \
                    CAST(COALESCE(SUM(insertions), 0) AS SIGNED) AS insertions, \
                    CAST(COALESCE(SUM(deletions), 0) AS SIGNED) AS deletions, \
                    MAX(committed_at) AS last_commit_at \
                 FROM commit_file_changes \
                 WHERE project_id = ? \
                 GROUP BY file_path \
                 ORDER BY commit_count DESC, last_commit_at DESC, file_path ASC \
                 LIMIT ?",
            )
            .bind(project_id)
            .bind(limit)
            .fetch_all(self.db.pool())
            .await?,
        };
        Ok(rows)
    }

    /// Top `limit` co-edited file *pairs* project-wide, ranked by
    /// distinct-commit co-edit count.
    ///
    /// Callers over-fetch (the MCP dispatch passes 25× the user's
    /// limit, clamped at 500) so a Rust-side exclusion filter can drop
    /// matches without starving the returned set. The underlying SQL
    /// aggregation is invariant to `LIMIT` — the sort is the work — so
    /// over-fetch is effectively free.
    ///
    /// `max_files_per_commit` drops commits that touch more than N
    /// files before the pair-join. Without this, a single lockfile
    /// refresh of 200 files contributes ~20k pair-counts and swamps
    /// the real coupling signal; the default at call sites is 15.
    ///
    /// `since_days`: when `Some`, restricts the source commits to
    /// those with `committed_at >= now - N days` (ISO-8601 lexical
    /// compare, matching the `churn` op).
    pub async fn top_coupled_pairs(
        &self,
        project_id: &str,
        limit: usize,
        since: Option<&str>,
        _max_files_per_commit: usize,
    ) -> Result<Vec<CoupledPair>> {
        self.db.ensure_initialized().await?;
        let limit = limit.clamp(1, 5000) as i64;

        // Reads from `coupling_pair_events`. The big-commit cap that
        // `_max_files_per_commit` used to enforce is now applied at
        // ingest time (see `MAX_FILES_PER_COMMIT_FOR_PAIRS`), so the
        // parameter is accepted for backwards compatibility with the
        // RepoGraphOps trait surface but ignored here. Without the
        // self-join, this is a straight indexed range scan on
        // `idx_recent` (with `since`) or PK prefix (without).
        let rows: Vec<CoupledPair> = match since {
            Some(ts) => sqlx::query_as(
                "SELECT \
                    file_a, \
                    file_b, \
                    CAST(COUNT(*) AS SIGNED) AS co_edits, \
                    MAX(committed_at) AS last_co_edit \
                 FROM coupling_pair_events \
                 WHERE project_id = ? AND committed_at >= ? \
                 GROUP BY file_a, file_b \
                 ORDER BY co_edits DESC, last_co_edit DESC, file_a ASC, file_b ASC \
                 LIMIT ?",
            )
            .bind(project_id)
            .bind(ts)
            .bind(limit)
            .fetch_all(self.db.pool())
            .await?,
            None => sqlx::query_as(
                "SELECT \
                    file_a, \
                    file_b, \
                    CAST(COUNT(*) AS SIGNED) AS co_edits, \
                    MAX(committed_at) AS last_co_edit \
                 FROM coupling_pair_events \
                 WHERE project_id = ? \
                 GROUP BY file_a, file_b \
                 ORDER BY co_edits DESC, last_co_edit DESC, file_a ASC, file_b ASC \
                 LIMIT ?",
            )
            .bind(project_id)
            .bind(limit)
            .fetch_all(self.db.pool())
            .await?,
        };
        Ok(rows)
    }

    /// Raw pair-event read for ops that need per-event timestamps
    /// (e.g. exponential-decay weighted scores computed in Rust).
    /// Capped — pass a generous limit (the typical project keeps
    /// pair-event counts in the tens of thousands; scans of 50k rows
    /// stay sub-100ms over the indexed range).
    pub async fn pair_events(
        &self,
        project_id: &str,
        since: Option<&str>,
        limit: usize,
    ) -> Result<Vec<CouplingPairEvent>> {
        self.db.ensure_initialized().await?;
        let limit = limit.clamp(1, 200_000) as i64;
        let rows: Vec<CouplingPairEvent> = match since {
            Some(ts) => sqlx::query_as(
                "SELECT project_id, file_a, file_b, commit_sha, committed_at \
                 FROM coupling_pair_events \
                 WHERE project_id = ? AND committed_at >= ? \
                 ORDER BY committed_at DESC \
                 LIMIT ?",
            )
            .bind(project_id)
            .bind(ts)
            .bind(limit)
            .fetch_all(self.db.pool())
            .await?,
            None => sqlx::query_as(
                "SELECT project_id, file_a, file_b, commit_sha, committed_at \
                 FROM coupling_pair_events \
                 WHERE project_id = ? \
                 ORDER BY committed_at DESC \
                 LIMIT ?",
            )
            .bind(project_id)
            .bind(limit)
            .fetch_all(self.db.pool())
            .await?,
        };
        Ok(rows)
    }

    /// Top `limit` files by cumulative coupling across every partner,
    /// derived by fetching `fetch_pairs` pairs via
    /// [`Self::top_coupled_pairs`] and aggregating on the Rust side.
    ///
    /// `fetch_pairs` is the over-fetch knob that feeds the hub
    /// aggregation — pass something comfortably larger than
    /// `limit * (avg_partner_count)` so the totals stabilise. The MCP
    /// dispatch passes 2000 by default, which is plenty for hubs-of-20.
    pub async fn coupling_hubs(
        &self,
        project_id: &str,
        limit: usize,
        since: Option<&str>,
        max_files_per_commit: usize,
        fetch_pairs: usize,
    ) -> Result<Vec<CouplingHub>> {
        self.db.ensure_initialized().await?;
        let pairs = self
            .top_coupled_pairs(project_id, fetch_pairs, since, max_files_per_commit)
            .await?;

        use std::collections::HashMap;
        // (file_path -> (total_coupling, partner_count))
        let mut agg: HashMap<String, (i64, i64)> = HashMap::new();
        for pair in &pairs {
            let a = agg.entry(pair.file_a.clone()).or_insert((0, 0));
            a.0 = a.0.saturating_add(pair.co_edits);
            a.1 = a.1.saturating_add(1);
            let b = agg.entry(pair.file_b.clone()).or_insert((0, 0));
            b.0 = b.0.saturating_add(pair.co_edits);
            b.1 = b.1.saturating_add(1);
        }

        let mut hubs: Vec<CouplingHub> = agg
            .into_iter()
            .map(|(file_path, (total_coupling, partner_count))| CouplingHub {
                file_path,
                total_coupling,
                partner_count,
            })
            .collect();
        // Sort by total desc, partner_count desc, path asc for stable
        // output.
        hubs.sort_by(|x, y| {
            y.total_coupling
                .cmp(&x.total_coupling)
                .then_with(|| y.partner_count.cmp(&x.partner_count))
                .then_with(|| x.file_path.cmp(&y.file_path))
        });
        hubs.truncate(limit.max(1));
        Ok(hubs)
    }
}

/// Derive `CouplingPairEvent`s from a single commit's file list,
/// appending into `out`. Returns nothing — caller checks `out` if it
/// cares about the count. Skips commits with > MAX_FILES_PER_COMMIT_FOR_PAIRS
/// files (the bulk-rewrite filter that used to live in the query
/// layer; now applied at write time).
///
/// `files` does not need to be sorted; the function dedups + sorts so
/// `file_a < file_b` is enforced regardless of input order.
pub fn derive_pair_events_into(
    project_id: &str,
    commit_sha: &str,
    committed_at: &str,
    files: &[String],
    out: &mut Vec<CouplingPairEvent>,
) {
    if files.len() < 2 || files.len() > MAX_FILES_PER_COMMIT_FOR_PAIRS {
        return;
    }
    // Dedup + sort. A commit row may appear twice if a file was
    // both renamed and modified (edge case in the parser); sorting
    // also gives us file_a < file_b for free.
    let mut sorted: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.len() < 2 || sorted.len() > MAX_FILES_PER_COMMIT_FOR_PAIRS {
        return;
    }
    for i in 0..sorted.len() {
        for j in (i + 1)..sorted.len() {
            out.push(CouplingPairEvent {
                project_id: project_id.to_owned(),
                file_a: sorted[i].to_owned(),
                file_b: sorted[j].to_owned(),
                commit_sha: commit_sha.to_owned(),
                committed_at: committed_at.to_owned(),
            });
        }
    }
}

/// Convenience wrapper around [`derive_pair_events_into`] that
/// allocates a fresh vec. Callers in the ingest hot path should prefer
/// the `_into` variant to amortise allocation across commits.
pub fn derive_pair_events(
    project_id: &str,
    commit_sha: &str,
    committed_at: &str,
    files: &[String],
) -> Vec<CouplingPairEvent> {
    let mut out = Vec::new();
    derive_pair_events_into(project_id, commit_sha, committed_at, files, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> CommitFileChangeRepository {
        let db = Database::open_in_memory().expect("in-memory db");
        CommitFileChangeRepository::new(db)
    }

    fn row(
        project_id: &str,
        commit_sha: &str,
        file_path: &str,
        committed_at: &str,
    ) -> CommitFileChange {
        CommitFileChange {
            project_id: project_id.to_owned(),
            commit_sha: commit_sha.to_owned(),
            file_path: file_path.to_owned(),
            change_kind: "M".to_owned(),
            committed_at: committed_at.to_owned(),
            author_email: "t@t".to_owned(),
            insertions: 1,
            deletions: 0,
            old_path: None,
        }
    }

    #[tokio::test]
    async fn upsert_and_cursor_round_trip() {
        let repo = fresh();
        let rows = vec![
            row("p1", "abc", "src/a.rs", "2026-04-01T00:00:00Z"),
            row("p1", "abc", "src/b.rs", "2026-04-01T00:00:00Z"),
        ];
        let n = repo.upsert_batch(&rows).await.expect("upsert");
        assert_eq!(n, 2);

        assert!(repo.get_cursor("p1").await.expect("cursor").is_none());
        repo.set_cursor("p1", "abc").await.expect("set cursor");
        assert_eq!(
            repo.get_cursor("p1").await.expect("cursor").as_deref(),
            Some("abc")
        );

        // Idempotency — re-insert the same rows, cursor unchanged.
        let n = repo.upsert_batch(&rows).await.expect("reupsert");
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn top_coupled_ranks_by_distinct_commit_count() {
        let repo = fresh();
        // Commit 1: a.rs + b.rs  (a ↔ b : 1 shared)
        // Commit 2: a.rs + b.rs + c.rs (a ↔ b : 2, a ↔ c : 1)
        // Commit 3: a.rs + c.rs (a ↔ c : 2, a ↔ b unchanged)
        let commits = [
            ("c1", "2026-04-01T00:00:00Z", vec!["src/a.rs", "src/b.rs"]),
            (
                "c2",
                "2026-04-02T00:00:00Z",
                vec!["src/a.rs", "src/b.rs", "src/c.rs"],
            ),
            ("c3", "2026-04-03T00:00:00Z", vec!["src/a.rs", "src/c.rs"]),
        ];
        let mut rows = Vec::new();
        for (sha, ts, paths) in commits.iter() {
            for p in paths {
                rows.push(row("p1", sha, p, ts));
            }
        }
        repo.upsert_batch(&rows).await.expect("upsert");
        // Coupling queries read from `coupling_pair_events` (built at
        // ingest time); the test inserts raw `commit_file_changes`
        // rows so we trigger the same backfill path the warmer uses
        // on first run after migration 20.
        repo.rebuild_pair_events_for_project("p1")
            .await
            .expect("rebuild pairs");

        let coupled = repo
            .top_coupled("p1", "src/a.rs", 10)
            .await
            .expect("coupled");
        // a ↔ b : 2, a ↔ c : 2 — tie broken by last_co_edit DESC, then
        // file_path ASC. c's last co-edit is 2026-04-03 (newer), so c
        // comes first.
        assert_eq!(coupled.len(), 2);
        assert_eq!(coupled[0].file_path, "src/c.rs");
        assert_eq!(coupled[0].co_edit_count, 2);
        assert_eq!(coupled[1].file_path, "src/b.rs");
        assert_eq!(coupled[1].co_edit_count, 2);
        let samples = coupled[0].supporting_commit_samples();
        assert!(samples.contains(&"c3".to_owned()));
        assert!(samples.contains(&"c2".to_owned()));
    }

    #[tokio::test]
    async fn churn_counts_distinct_commits_per_file() {
        let repo = fresh();
        let mut rows = vec![
            CommitFileChange {
                project_id: "p1".into(),
                commit_sha: "c1".into(),
                file_path: "src/a.rs".into(),
                change_kind: "M".into(),
                committed_at: "2026-04-01T00:00:00Z".into(),
                author_email: "t@t".into(),
                insertions: 5,
                deletions: 1,
                old_path: None,
            },
            CommitFileChange {
                project_id: "p1".into(),
                commit_sha: "c2".into(),
                file_path: "src/a.rs".into(),
                change_kind: "M".into(),
                committed_at: "2026-04-02T00:00:00Z".into(),
                author_email: "t@t".into(),
                insertions: 10,
                deletions: 2,
                old_path: None,
            },
        ];
        rows.push(CommitFileChange {
            project_id: "p1".into(),
            commit_sha: "c3".into(),
            file_path: "src/b.rs".into(),
            change_kind: "M".into(),
            committed_at: "2026-04-03T00:00:00Z".into(),
            author_email: "t@t".into(),
            insertions: 1,
            deletions: 0,
            old_path: None,
        });
        repo.upsert_batch(&rows).await.expect("upsert");

        let all = repo.churn("p1", 10, None).await.expect("churn");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].file_path, "src/a.rs");
        assert_eq!(all[0].commit_count, 2);
        assert_eq!(all[0].insertions, 15);
        assert_eq!(all[0].deletions, 3);
        assert_eq!(all[1].file_path, "src/b.rs");
        assert_eq!(all[1].commit_count, 1);

        // Since filter: only count commits on or after 2026-04-02.
        let recent = repo
            .churn("p1", 10, Some("2026-04-02T00:00:00Z"))
            .await
            .expect("churn");
        let a = recent.iter().find(|r| r.file_path == "src/a.rs").unwrap();
        assert_eq!(a.commit_count, 1);
    }

    #[tokio::test]
    async fn upsert_empty_is_noop() {
        let repo = fresh();
        let n = repo.upsert_batch(&[]).await.expect("noop");
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn top_coupled_pairs_ranks_and_dedups_pairs() {
        let repo = fresh();
        // c1: a+b  (pair a↔b : 1)
        // c2: a+b+c (a↔b : 2, a↔c : 1, b↔c : 1)
        // c3: a+c  (a↔c : 2)
        let commits: [(&str, &str, Vec<&str>); 3] = [
            ("c1", "2026-04-01T00:00:00Z", vec!["src/a.rs", "src/b.rs"]),
            (
                "c2",
                "2026-04-02T00:00:00Z",
                vec!["src/a.rs", "src/b.rs", "src/c.rs"],
            ),
            ("c3", "2026-04-03T00:00:00Z", vec!["src/a.rs", "src/c.rs"]),
        ];
        let mut rows = Vec::new();
        for (sha, ts, paths) in commits.iter() {
            for p in paths {
                rows.push(row("p1", sha, p, ts));
            }
        }
        repo.upsert_batch(&rows).await.expect("upsert");
        repo.rebuild_pair_events_for_project("p1")
            .await
            .expect("rebuild pairs");

        let pairs = repo
            .top_coupled_pairs("p1", 100, None, 15)
            .await
            .expect("pairs");
        // Three unordered pairs: a↔b=2, a↔c=2, b↔c=1
        assert_eq!(pairs.len(), 3);
        // Canonical ordering means a<b, a<c, b<c.
        let ab = pairs
            .iter()
            .find(|p| p.file_a == "src/a.rs" && p.file_b == "src/b.rs")
            .expect("a↔b");
        let ac = pairs
            .iter()
            .find(|p| p.file_a == "src/a.rs" && p.file_b == "src/c.rs")
            .expect("a↔c");
        let bc = pairs
            .iter()
            .find(|p| p.file_a == "src/b.rs" && p.file_b == "src/c.rs")
            .expect("b↔c");
        assert_eq!(ab.co_edits, 2);
        assert_eq!(ac.co_edits, 2);
        assert_eq!(bc.co_edits, 1);
    }

    #[tokio::test]
    async fn top_coupled_pairs_skips_big_commits() {
        let repo = fresh();
        // A small real commit (a+b) plus a "lockfile refresh"-style
        // commit touching 16 files. The cap (`MAX_FILES_PER_COMMIT_FOR_PAIRS`
        // = 15) is now applied at ingest time, so the big commit
        // contributes zero pairs regardless of query parameters.
        let mut rows = Vec::new();
        for p in ["src/a.rs", "src/b.rs"] {
            rows.push(row("p1", "small", p, "2026-04-01T00:00:00Z"));
        }
        let big_files: Vec<String> =
            (0..16).map(|i| format!("src/big_{i:02}.rs")).collect();
        // Make sure a + b are also touched in the big commit so a
        // naive (no-cap) implementation would over-count a↔b at 2.
        let big_with_ab: Vec<String> = std::iter::once("src/a.rs".to_string())
            .chain(std::iter::once("src/b.rs".to_string()))
            .chain(big_files.iter().cloned())
            .collect();
        for p in &big_with_ab {
            rows.push(row("p1", "big", p, "2026-04-02T00:00:00Z"));
        }
        repo.upsert_batch(&rows).await.expect("upsert");
        repo.rebuild_pair_events_for_project("p1")
            .await
            .expect("rebuild pairs");

        // The big commit was dropped at write time. a↔b only has the
        // small commit's contribution = 1, and none of the big_NN
        // files appear as pairs anywhere.
        let pairs = repo
            .top_coupled_pairs("p1", 100, None, 15)
            .await
            .expect("pairs");
        let ab = pairs
            .iter()
            .find(|p| p.file_a == "src/a.rs" && p.file_b == "src/b.rs")
            .expect("a↔b");
        assert_eq!(ab.co_edits, 1);
        assert!(
            !pairs.iter().any(|p| p.file_a.starts_with("src/big_")
                || p.file_b.starts_with("src/big_")),
            "big commit files leaked into pair table"
        );
    }

    #[test]
    fn derive_pair_events_enforces_cap_and_canonical_order() {
        // 2 files → 1 pair, file_a < file_b regardless of input order.
        let events = derive_pair_events(
            "p1",
            "abc",
            "2026-04-01T00:00:00Z",
            &["src/z.rs".to_string(), "src/a.rs".to_string()],
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].file_a, "src/a.rs");
        assert_eq!(events[0].file_b, "src/z.rs");

        // 1 file → no pair (commit must touch ≥2 to couple).
        assert!(
            derive_pair_events("p1", "abc", "ts", &["only.rs".to_string()]).is_empty()
        );

        // > MAX_FILES_PER_COMMIT_FOR_PAIRS files → no pairs (cap drops
        // bulk rewrites at write time so the read path stays fast).
        let too_many: Vec<String> = (0..(MAX_FILES_PER_COMMIT_FOR_PAIRS + 1))
            .map(|i| format!("f{i}.rs"))
            .collect();
        assert!(
            derive_pair_events("p1", "abc", "ts", &too_many).is_empty(),
            "commit with {} files should be excluded by cap (max {})",
            too_many.len(),
            MAX_FILES_PER_COMMIT_FOR_PAIRS
        );

        // Exactly at the cap: emits all C(n, 2) pairs.
        let at_cap: Vec<String> = (0..MAX_FILES_PER_COMMIT_FOR_PAIRS)
            .map(|i| format!("f{i:02}.rs"))
            .collect();
        let n = MAX_FILES_PER_COMMIT_FOR_PAIRS;
        let expected = n * (n - 1) / 2;
        assert_eq!(derive_pair_events("p1", "abc", "ts", &at_cap).len(), expected);
    }

    #[test]
    fn derive_pair_events_dedups_duplicate_paths() {
        // Parser may emit a duplicate row for rename+modify edge case;
        // dedup so we don't get a spurious self-pair.
        let events = derive_pair_events(
            "p1",
            "abc",
            "ts",
            &["src/a.rs".to_string(), "src/a.rs".to_string(), "src/b.rs".to_string()],
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].file_a, "src/a.rs");
        assert_eq!(events[0].file_b, "src/b.rs");
    }

    #[tokio::test]
    async fn rebuild_matches_live_ingest() {
        // Fresh repo: rows inserted via `upsert_batch`, then
        // `rebuild_pair_events_for_project` should produce the same
        // pair events as if we had derived them at ingest time.
        let repo = fresh();
        let commits: [(&str, &str, Vec<&str>); 2] = [
            ("c1", "2026-04-01T00:00:00Z", vec!["src/a.rs", "src/b.rs"]),
            (
                "c2",
                "2026-04-02T00:00:00Z",
                vec!["src/b.rs", "src/c.rs", "src/d.rs"],
            ),
        ];
        let mut rows = Vec::new();
        for (sha, ts, paths) in commits.iter() {
            for p in paths {
                rows.push(row("p1", sha, p, ts));
            }
        }
        repo.upsert_batch(&rows).await.expect("upsert");
        repo.rebuild_pair_events_for_project("p1")
            .await
            .expect("rebuild");

        // c1 → 1 pair (a↔b), c2 → 3 pairs (b↔c, b↔d, c↔d) = 4 events.
        assert_eq!(
            repo.pair_events_count_for_project("p1")
                .await
                .expect("count"),
            4
        );

        // Idempotent — second rebuild produces the same row set.
        repo.rebuild_pair_events_for_project("p1")
            .await
            .expect("rebuild again");
        assert_eq!(
            repo.pair_events_count_for_project("p1")
                .await
                .expect("count again"),
            4
        );
    }

    #[tokio::test]
    async fn coupling_hubs_aggregates_bidirectionally() {
        let repo = fresh();
        // Build a "hub" shape: a is co-edited with b, c, d; b and c
        // only share with a.
        let commits: [(&str, &str, Vec<&str>); 3] = [
            ("c1", "2026-04-01T00:00:00Z", vec!["src/a.rs", "src/b.rs"]),
            ("c2", "2026-04-02T00:00:00Z", vec!["src/a.rs", "src/c.rs"]),
            ("c3", "2026-04-03T00:00:00Z", vec!["src/a.rs", "src/d.rs"]),
        ];
        let mut rows = Vec::new();
        for (sha, ts, paths) in commits.iter() {
            for p in paths {
                rows.push(row("p1", sha, p, ts));
            }
        }
        repo.upsert_batch(&rows).await.expect("upsert");
        repo.rebuild_pair_events_for_project("p1")
            .await
            .expect("rebuild pairs");

        let hubs = repo
            .coupling_hubs("p1", 10, None, 15, 2000)
            .await
            .expect("hubs");
        // a.rs pairs with b, c, d → total_coupling=3, partner_count=3.
        let a = hubs
            .iter()
            .find(|h| h.file_path == "src/a.rs")
            .expect("hub a");
        assert_eq!(a.total_coupling, 3);
        assert_eq!(a.partner_count, 3);
        // Spokes — each appears in one pair only.
        for spoke in ["src/b.rs", "src/c.rs", "src/d.rs"] {
            let h = hubs.iter().find(|h| h.file_path == spoke).unwrap();
            assert_eq!(h.total_coupling, 1);
            assert_eq!(h.partner_count, 1);
        }
        // a.rs wins on total.
        assert_eq!(hubs[0].file_path, "src/a.rs");
    }
}
