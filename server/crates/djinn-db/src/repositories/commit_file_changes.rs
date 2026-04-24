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
    pub async fn top_coupled(
        &self,
        project_id: &str,
        file_path: &str,
        limit: usize,
    ) -> Result<Vec<CoupledFile>> {
        self.db.ensure_initialized().await?;
        // Self-join on commit_sha: for every commit that touched the
        // seed file, pull every other file touched in the same commit.
        // GROUP BY peer path → distinct commit count + most recent
        // co-edit timestamp + up to three sample SHAs.
        let limit = limit.clamp(1, 500) as i64;
        let rows: Vec<CoupledFile> = sqlx::query_as(
            "SELECT \
                peer.file_path AS file_path, \
                CAST(COUNT(DISTINCT peer.commit_sha) AS SIGNED) AS co_edit_count, \
                MAX(peer.committed_at) AS last_co_edit, \
                GROUP_CONCAT(DISTINCT peer.commit_sha ORDER BY peer.committed_at DESC SEPARATOR ',') AS supporting_commit_samples \
             FROM commit_file_changes AS seed \
             JOIN commit_file_changes AS peer \
               ON peer.project_id = seed.project_id \
              AND peer.commit_sha = seed.commit_sha \
              AND peer.file_path <> seed.file_path \
             WHERE seed.project_id = ? AND seed.file_path = ? \
             GROUP BY peer.file_path \
             ORDER BY co_edit_count DESC, last_co_edit DESC, peer.file_path ASC \
             LIMIT ?",
        )
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
}
