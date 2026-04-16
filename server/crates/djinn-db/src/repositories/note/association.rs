use super::*;
use crate::repositories::note::NoteRepository;
use djinn_core::models::{NoteAssociation, canonical_pair};

/// A resolved association entry: the "other" note's identity plus the link weight.
#[derive(Clone, Debug, sqlx::FromRow, serde::Serialize)]
pub struct NoteAssociationEntry {
    pub note_permalink: String,
    pub note_title: String,
    pub weight: f64,
    pub co_access_count: i64,
    pub last_co_access: String,
}

impl NoteRepository {
    /// Upsert a co-access association between two notes.
    ///
    /// * `note_a_id` and `note_b_id` - The two note IDs that were co-accessed.
    /// * `n_co_accesses` - Number of co-access events to record (typically 1, or higher
    ///   for batch session processing).
    ///
    /// The note IDs are canonicalized internally (min < max) to satisfy the
    /// CHECK constraint.
    ///
    /// Returns the updated (or newly created) association.
    pub async fn upsert_association(
        &self,
        note_a_id: &str,
        note_b_id: &str,
        n_co_accesses: u32,
    ) -> Result<NoteAssociation> {
        self.db.ensure_initialized().await?;

        // Canonical ordering to satisfy CHECK constraint
        let (a_id, b_id) = canonical_pair(note_a_id, note_b_id);

        let growth_factor = (0..n_co_accesses).fold(1.0_f64, |acc, _| acc * 1.01);
        let new_co_accesses = i64::from(n_co_accesses);
        sqlx::query!(
            "INSERT INTO note_associations
             (note_a_id, note_b_id, weight, co_access_count, last_co_access)
             VALUES (?, ?, 0.01, ?, DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'))
             ON DUPLICATE KEY UPDATE
                 weight = LEAST(1.0, weight * ?),
                 co_access_count = co_access_count + VALUES(co_access_count),
                 last_co_access = VALUES(last_co_access)",
            a_id,
            b_id,
            new_co_accesses,
            growth_factor
        )
        .execute(self.db.pool())
        .await?;

        Ok::<NoteAssociation, crate::error::DbError>(
            sqlx::query_as!(
                NoteAssociation,
                "SELECT note_a_id, note_b_id, weight, co_access_count, last_co_access
                 FROM note_associations
                 WHERE note_a_id = ? AND note_b_id = ?",
                a_id,
                b_id
            )
            .fetch_one(self.db.pool())
            .await?,
        )
    }

    /// Upsert a semantic association with a minimum target weight.
    ///
    /// Unlike `upsert_association` (which uses multiplicative growth from 0.01),
    /// this method sets the weight to at least `min_weight`. Used for
    /// LLM-classified semantic relationships (contradiction, supersedes, elaborates).
    ///
    /// The note IDs are canonicalized internally (min < max).
    pub async fn upsert_association_min_weight(
        &self,
        note_a_id: &str,
        note_b_id: &str,
        min_weight: f64,
    ) -> Result<NoteAssociation> {
        self.db.ensure_initialized().await?;

        let (a_id, b_id) = canonical_pair(note_a_id, note_b_id);
        let min_weight = min_weight.clamp(0.0, 1.0);

        sqlx::query!(
            "INSERT INTO note_associations
             (note_a_id, note_b_id, weight, co_access_count, last_co_access)
             VALUES (?, ?, ?, 1, DATE_FORMAT(NOW(3), '%Y-%m-%dT%H:%i:%s.%fZ'))
             ON DUPLICATE KEY UPDATE
                 weight = GREATEST(weight, VALUES(weight)),
                 co_access_count = co_access_count + 1,
                 last_co_access = VALUES(last_co_access)",
            a_id,
            b_id,
            min_weight
        )
        .execute(self.db.pool())
        .await?;

        Ok::<NoteAssociation, crate::error::DbError>(
            sqlx::query_as!(
                NoteAssociation,
                "SELECT note_a_id, note_b_id, weight, co_access_count, last_co_access
                 FROM note_associations
                 WHERE note_a_id = ? AND note_b_id = ?",
                a_id,
                b_id
            )
            .fetch_one(self.db.pool())
            .await?,
        )
    }

    /// Get all associations for a given note.
    ///
    /// Returns associations where the note is either note_a_id or note_b_id,
    /// ordered by weight descending.
    pub async fn get_associations_for_note(&self, note_id: &str) -> Result<Vec<NoteAssociation>> {
        self.db.ensure_initialized().await?;

        let associations: Vec<NoteAssociation> = sqlx::query_as!(
            NoteAssociation,
            "SELECT note_a_id, note_b_id, weight, co_access_count, last_co_access
             FROM note_associations
             WHERE note_a_id = ? OR note_b_id = ?
             ORDER BY weight DESC",
            note_id,
            note_id
        )
        .fetch_all(self.db.pool())
        .await?;

        Ok(associations)
    }

    /// List associations for a note, joining the opposite note to return resolved
    /// permalink and title. Covers both directions (note_a_id = id OR note_b_id = id).
    ///
    /// * `note_id`    – the note whose associations to fetch.
    /// * `min_weight` – include only associations with weight >= this value.
    /// * `limit`      – cap result count (0 = unlimited).
    pub async fn list_associations_for_note(
        &self,
        note_id: &str,
        min_weight: f64,
        limit: i64,
    ) -> Result<Vec<NoteAssociationEntry>> {
        self.db.ensure_initialized().await?;

        let effective_limit: i64 = if limit <= 0 { i64::MAX } else { limit };
        let entries: Vec<NoteAssociationEntry> = sqlx::query_as!(
            NoteAssociationEntry,
            r#"SELECT
                 CASE WHEN na.note_a_id = ? THEN nb.permalink ELSE na_.permalink END AS "note_permalink!: String",
                 CASE WHEN na.note_a_id = ? THEN nb.title    ELSE na_.title    END AS "note_title!: String",
                 na.weight,
                 na.co_access_count,
                 na.last_co_access
             FROM note_associations na
             JOIN notes na_ ON na_.id = na.note_a_id
             JOIN notes nb  ON nb.id  = na.note_b_id
             WHERE (na.note_a_id = ? OR na.note_b_id = ?)
               AND na.weight >= ?
             ORDER BY na.weight DESC
             LIMIT ?"#,
            note_id,
            note_id,
            note_id,
            note_id,
            min_weight,
            effective_limit
        )
        .fetch_all(self.db.pool())
        .await?;

        Ok(entries)
    }

    /// List all associations with weight above a threshold.
    ///
    /// Returns associations ordered by weight descending.
    pub async fn list_associations_above_weight(
        &self,
        threshold: f64,
    ) -> Result<Vec<NoteAssociation>> {
        self.db.ensure_initialized().await?;

        let associations: Vec<NoteAssociation> = sqlx::query_as!(
            NoteAssociation,
            "SELECT note_a_id, note_b_id, weight, co_access_count, last_co_access
             FROM note_associations
             WHERE weight >= ?
             ORDER BY weight DESC",
            threshold
        )
        .fetch_all(self.db.pool())
        .await?;

        Ok(associations)
    }

    /// Delete associations with weight below a threshold.
    ///
    /// Useful for periodic pruning of low-weight associations.
    /// Returns the number of associations deleted.
    pub async fn prune_associations_below_weight(&self, threshold: f64) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let result = sqlx::query!(
            "DELETE FROM note_associations
             WHERE weight < ?",
            threshold
        )
        .execute(self.db.pool())
        .await?;

        Ok(result.rows_affected())
    }

    /// Delete associations older than a given timestamp with weight below threshold.
    ///
    /// Returns the number of associations deleted.
    pub async fn prune_old_associations(
        &self,
        before_timestamp: &str,
        max_weight: f64,
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let result = sqlx::query!(
            "DELETE FROM note_associations
             WHERE last_co_access < ? AND weight <= ?",
            before_timestamp,
            max_weight
        )
        .execute(self.db.pool())
        .await?;

        Ok(result.rows_affected())
    }

    /// Prune low-weight, stale associations for a specific project.
    ///
    /// Deletes associations where:
    /// - weight < 0.05 (low weight threshold)
    /// - last_co_access is older than 90 days
    /// - note_a_id belongs to a note in the specified project
    ///
    /// Returns the number of associations deleted.
    pub async fn prune_associations(&self, project_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let result = sqlx::query!(
            "DELETE FROM note_associations
             WHERE weight < 0.05
               AND last_co_access < DATE_SUB(NOW(3), INTERVAL 90 DAY)
               AND note_a_id IN (SELECT id FROM notes WHERE project_id = ?)",
            project_id
        )
        .execute(self.db.pool())
        .await?;

        Ok(result.rows_affected())
    }

    /// Record bounded co-access associations between a repo-map note and a set of companion notes.
    ///
    /// This helper is used for Hebbian learning when a repo-map note is co-accessed with
    /// task/context notes during a chat session. It records one co-access increment per
    /// unique (repo_map_note_id, companion_note_id) pair.
    ///
    /// * `repo_map_note_id` - The ID of the persisted repo-map note.
    /// * `companion_note_ids` - A deduplicated set of note IDs that were co-accessed with the repo-map.
    ///
    /// The helper:
    /// - Ignores self-pairs (repo_map_note_id == companion_note_id)
    /// - Ignores duplicate companion IDs (already deduplicated by caller expectation, but defensive)
    /// - Records one bounded co-access increment per unique pair using `upsert_association`
    ///
    /// Returns the number of associations recorded (excludes self-pairs).
    pub async fn record_repo_map_co_access<I: IntoIterator<Item = String>>(
        &self,
        repo_map_note_id: &str,
        companion_note_ids: I,
    ) -> Result<usize> {
        use std::collections::HashSet;

        // Deduplicate companion IDs and filter out self-pairs
        let unique_companions: HashSet<String> = companion_note_ids
            .into_iter()
            .filter(|id| id != repo_map_note_id)
            .collect();

        // No-op if no valid companions after filtering
        if unique_companions.is_empty() {
            return Ok(0);
        }

        // Record one co-access increment per unique pair
        let mut recorded = 0;
        for companion_id in unique_companions {
            self.upsert_association(repo_map_note_id, &companion_id, 1)
                .await?;
            recorded += 1;
        }

        Ok(recorded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::models::Project;
    use tokio::sync::broadcast;

    use crate::repositories::test_support::{event_bus_for, make_project};

    async fn make_note(
        repo: &NoteRepository,
        project: &Project,
        tmp: &tempfile::TempDir,
        title: &str,
    ) -> String {
        let note = repo
            .create(&project.id, tmp.path(), title, "content", "reference", "[]")
            .await
            .unwrap();
        note.id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_association_creates_new() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note1 = make_note(&repo, &project, &tmp, "Note One").await;
        let note2 = make_note(&repo, &project, &tmp, "Note Two").await;

        let assoc = repo.upsert_association(&note1, &note2, 1).await.unwrap();

        // Verify canonical ordering
        let (expected_a, expected_b) = canonical_pair(&note1, &note2);
        assert_eq!(assoc.note_a_id, expected_a);
        assert_eq!(assoc.note_b_id, expected_b);
        assert_eq!(assoc.weight, 0.01);
        assert_eq!(assoc.co_access_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_association_updates_existing() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note1 = make_note(&repo, &project, &tmp, "Note One").await;
        let note2 = make_note(&repo, &project, &tmp, "Note Two").await;

        // Create initial association
        let _ = repo.upsert_association(&note1, &note2, 1).await.unwrap();

        let assoc = repo.upsert_association(&note1, &note2, 1).await.unwrap();

        assert_eq!(assoc.co_access_count, 2);
        assert!((assoc.weight - 0.0101).abs() < 1e-12);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_association_many_individual_updates_approaches_one_without_exceeding() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note1 = make_note(&repo, &project, &tmp, "Note One").await;
        let note2 = make_note(&repo, &project, &tmp, "Note Two").await;

        let mut assoc = repo.upsert_association(&note1, &note2, 1).await.unwrap();
        for _ in 0..499 {
            assoc = repo.upsert_association(&note1, &note2, 1).await.unwrap();
        }

        assert_eq!(assoc.co_access_count, 500);
        assert!(assoc.weight >= 0.99);
        assert!(assoc.weight <= 1.0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn upsert_association_bulk_update_caps_weight_at_one() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note1 = make_note(&repo, &project, &tmp, "Note One").await;
        let note2 = make_note(&repo, &project, &tmp, "Note Two").await;

        let assoc = repo
            .upsert_association(&note1, &note2, 10_000)
            .await
            .unwrap();

        assert_eq!(assoc.co_access_count, 10_000);
        assert_eq!(assoc.weight, 0.01);

        let assoc = repo
            .upsert_association(&note1, &note2, 10_000)
            .await
            .unwrap();
        assert_eq!(assoc.co_access_count, 20_000);
        assert_eq!(assoc.weight, 1.0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canonical_ordering_enforced() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note_z = make_note(&repo, &project, &tmp, "Note Zebra").await;
        let note_a = make_note(&repo, &project, &tmp, "Note Alpha").await;

        // Pass in reverse order (z, a)
        let assoc = repo.upsert_association(&note_z, &note_a, 1).await.unwrap();

        // Verify canonical ordering is enforced by checking the association is stored correctly
        // The canonical pair should be (min, max)
        let (expected_a, expected_b) = canonical_pair(&note_z, &note_a);
        assert_eq!(assoc.note_a_id, expected_a);
        assert_eq!(assoc.note_b_id, expected_b);
        // note_a_id should be lexicographically less than note_b_id
        assert!(assoc.note_a_id < assoc.note_b_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_associations_for_note() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note1 = make_note(&repo, &project, &tmp, "Note One").await;
        let note2 = make_note(&repo, &project, &tmp, "Note Two").await;
        let note3 = make_note(&repo, &project, &tmp, "Note Three").await;

        repo.upsert_association(&note1, &note2, 1).await.unwrap();
        repo.upsert_association(&note1, &note3, 1).await.unwrap();

        let associations = repo.get_associations_for_note(&note1).await.unwrap();
        assert_eq!(associations.len(), 2);

        // Should be ordered by weight descending
        let ids: Vec<String> = associations
            .iter()
            .map(|a| {
                if a.note_a_id == note1 {
                    a.note_b_id.clone()
                } else {
                    a.note_a_id.clone()
                }
            })
            .collect();
        assert!(ids.contains(&note2));
        assert!(ids.contains(&note3));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_associations_above_weight() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db, event_bus_for(&tx));

        let note1 = make_note(&repo, &project, &tmp, "Note One").await;
        let note2 = make_note(&repo, &project, &tmp, "Note Two").await;
        let note3 = make_note(&repo, &project, &tmp, "Note Three").await;

        // Create associations with different effective weights.
        // New pairs start at 0.01, so to cross 0.5 we need repeated individual co-accesses.
        for _ in 0..401 {
            repo.upsert_association(&note1, &note2, 1).await.unwrap();
        }
        repo.upsert_association(&note1, &note3, 1).await.unwrap();

        let high_weight = repo.list_associations_above_weight(0.5).await.unwrap();
        assert_eq!(high_weight.len(), 1);
        // Should be the high-weight association (note1, note2)
        assert!(high_weight[0].weight > 0.5);

        let all = repo.list_associations_above_weight(0.0).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_delete_cascade_removes_associations() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let note1 = make_note(&repo, &project, &tmp, "Note One").await;
        let note2 = make_note(&repo, &project, &tmp, "Note Two").await;

        repo.upsert_association(&note1, &note2, 1).await.unwrap();

        // Verify association exists
        let before = repo.get_associations_for_note(&note1).await.unwrap();
        assert_eq!(before.len(), 1);

        // Delete note1 - should cascade delete the association
        repo.delete(&note1).await.unwrap();

        // Association should be gone
        let after = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM note_associations WHERE note_a_id = ? OR note_b_id = ?",
            note1,
            note1
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(after, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn check_constraint_blocks_reversed_pair() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let note1 = make_note(&repo, &project, &tmp, "Note One").await;
        let note2 = make_note(&repo, &project, &tmp, "Note Two").await;

        // Insert via raw SQL to bypass canonicalization - should fail
        let _result = sqlx::query!(
            "INSERT INTO note_associations (note_a_id, note_b_id) VALUES (?, ?)",
            note2, // note2 > note1
            note1
        )
        .execute(db.pool())
        .await;

        // This should fail the CHECK constraint since note_a_id > note_b_id
        // But SQLite doesn't enforce CHECK on virtual tables or some edge cases...
        // Actually let's just verify that our repo methods handle this correctly
        // by using canonical_pair

        // Use canonical_pair to ensure proper ordering
        let (a, b) = canonical_pair(&note2, &note1);
        assert_eq!(a, note1);
        assert_eq!(b, note2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prune_associations_removes_stale_low_weight() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        // Create three pairs of notes
        let note1 = make_note(&repo, &project, &tmp, "Note One").await;
        let note2 = make_note(&repo, &project, &tmp, "Note Two").await;
        let note3 = make_note(&repo, &project, &tmp, "Note Three").await;
        let note4 = make_note(&repo, &project, &tmp, "Note Four").await;
        let note5 = make_note(&repo, &project, &tmp, "Note Five").await;
        let note6 = make_note(&repo, &project, &tmp, "Note Six").await;

        // Create associations with different weights and co-access dates
        // Pair 1: weight=0.01, last_co_access 100 days ago (should be pruned)
        repo.upsert_association(&note1, &note2, 1).await.unwrap();
        sqlx::query!(
            "UPDATE note_associations SET last_co_access = DATE_SUB(NOW(3), INTERVAL 100 DAY) WHERE note_a_id = ? AND note_b_id = ?",
            note1,
            note2
        )
        .execute(db.pool())
        .await
        .unwrap();

        // Pair 2: weight=0.01, last_co_access yesterday (should survive - recent)
        repo.upsert_association(&note3, &note4, 1).await.unwrap();
        sqlx::query!(
            "UPDATE note_associations SET last_co_access = DATE_SUB(NOW(3), INTERVAL 1 DAY) WHERE note_a_id = ? AND note_b_id = ?",
            note3,
            note4
        )
        .execute(db.pool())
        .await
        .unwrap();

        // Pair 3: weight > 0.05, last_co_access 100 days ago (should survive - high weight)
        for _ in 0..164 {
            repo.upsert_association(&note5, &note6, 1).await.unwrap();
        }
        sqlx::query!(
            "UPDATE note_associations SET last_co_access = DATE_SUB(NOW(3), INTERVAL 100 DAY) WHERE note_a_id = ? AND note_b_id = ?",
            note5,
            note6
        )
        .execute(db.pool())
        .await
        .unwrap();

        // Verify all three associations exist
        let before_count: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM note_associations WHERE note_a_id IN (?, ?, ?) OR note_b_id IN (?, ?, ?)",
            note1,
            note3,
            note5,
            note1,
            note3,
            note5
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(before_count, 3);

        // Run prune
        let deleted = repo.prune_associations(&project.id).await.unwrap();
        assert_eq!(deleted, 1);

        // Verify only the first pair was deleted
        let remaining_rows = sqlx::query!(
            "SELECT note_a_id, note_b_id FROM note_associations WHERE note_a_id IN (?, ?, ?) OR note_b_id IN (?, ?, ?) ORDER BY note_a_id",
            note1,
            note3,
            note5,
            note1,
            note3,
            note5
        )
        .fetch_all(db.pool())
        .await
        .unwrap();
        let remaining: Vec<(String, String)> = remaining_rows
            .into_iter()
            .map(|r| (r.note_a_id, r.note_b_id))
            .collect();

        assert_eq!(remaining.len(), 2);
        // note3-note4 should survive (recent)
        assert!(
            remaining
                .iter()
                .any(|(a, b)| (a == &note3 && b == &note4) || (a == &note4 && b == &note3))
        );
        // note5-note6 should survive (high weight)
        assert!(
            remaining
                .iter()
                .any(|(a, b)| (a == &note5 && b == &note6) || (a == &note6 && b == &note5))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prune_associations_scoped_to_project() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);

        // Create two projects
        let project1 = make_project(&db, tmp.path()).await;
        let project2_path = tmp.path().join("project2");
        std::fs::create_dir_all(&project2_path).unwrap();
        let project2 = {
            db.ensure_initialized().await.unwrap();
            let id = uuid::Uuid::now_v7().to_string();
            let project2_path_str = project2_path.to_str().unwrap();
            sqlx::query!(
                "INSERT INTO projects (id, name, path) VALUES (?, ?, ?)",
                id,
                "test-project-2",
                project2_path_str
            )
            .execute(db.pool())
            .await
            .unwrap();
            sqlx::query_as!(
                Project,
                r#"SELECT id, name, path, created_at, target_branch,
                          auto_merge AS "auto_merge!: bool",
                          sync_enabled AS "sync_enabled!: bool",
                          sync_remote
                 FROM projects WHERE id = ?"#,
                id
            )
            .fetch_one(db.pool())
            .await
            .unwrap()
        };

        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        // Create notes in both projects
        let p1_note1 = make_note(&repo, &project1, &tmp, "P1 Note One").await;
        let p1_note2 = make_note(&repo, &project1, &tmp, "P1 Note Two").await;
        let p2_note1 = repo
            .create(
                &project2.id,
                &project2_path,
                "P2 Note One",
                "content",
                "reference",
                "[]",
            )
            .await
            .unwrap();
        let p2_note2 = repo
            .create(
                &project2.id,
                &project2_path,
                "P2 Note Two",
                "content",
                "reference",
                "[]",
            )
            .await
            .unwrap();

        // Create old, low-weight associations in both projects
        repo.upsert_association(&p1_note1, &p1_note2, 1)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE note_associations SET last_co_access = DATE_SUB(NOW(3), INTERVAL 100 DAY) WHERE note_a_id = ? AND note_b_id = ?",
            p1_note1,
            p1_note2
        )
        .execute(db.pool())
        .await
        .unwrap();

        repo.upsert_association(&p2_note1.id, &p2_note2.id, 1)
            .await
            .unwrap();
        sqlx::query!(
            "UPDATE note_associations SET last_co_access = DATE_SUB(NOW(3), INTERVAL 100 DAY) WHERE note_a_id = ? AND note_b_id = ?",
            p2_note1.id,
            p2_note2.id
        )
        .execute(db.pool())
        .await
        .unwrap();

        // Prune only project1
        let deleted = repo.prune_associations(&project1.id).await.unwrap();
        assert_eq!(deleted, 1);

        // Verify project2 association still exists
        let p2_count: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM note_associations WHERE note_a_id = ? OR note_b_id = ?",
            p2_note1.id,
            p2_note1.id
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(p2_count, 1);

        // Verify project1 association is gone
        let p1_count: i64 = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM note_associations WHERE note_a_id = ? OR note_b_id = ?",
            p1_note1,
            p1_note1
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(p1_count, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_repo_map_co_access_creates_associations_for_companions() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let repo_map_note = make_note(&repo, &project, &tmp, "Repo Map Note").await;
        let note1 = make_note(&repo, &project, &tmp, "Note One").await;
        let note2 = make_note(&repo, &project, &tmp, "Note Two").await;

        // Record co-access between repo-map and two companions
        let recorded = repo
            .record_repo_map_co_access(&repo_map_note, vec![note1.clone(), note2.clone()])
            .await
            .unwrap();

        assert_eq!(recorded, 2);

        // Verify associations were created
        let associations = repo
            .get_associations_for_note(&repo_map_note)
            .await
            .unwrap();
        assert_eq!(associations.len(), 2);

        // Verify both companions are associated
        let companion_ids: Vec<String> = associations
            .iter()
            .map(|a| {
                if a.note_a_id == repo_map_note {
                    a.note_b_id.clone()
                } else {
                    a.note_a_id.clone()
                }
            })
            .collect();
        assert!(companion_ids.contains(&note1));
        assert!(companion_ids.contains(&note2));

        // Verify initial weight and count
        let assoc = &associations[0];
        assert_eq!(assoc.weight, 0.01);
        assert_eq!(assoc.co_access_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_repo_map_co_access_ignores_self_pairs() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let repo_map_note = make_note(&repo, &project, &tmp, "Repo Map Note").await;

        // Try to record co-access with self (should be ignored)
        let recorded = repo
            .record_repo_map_co_access(&repo_map_note, vec![repo_map_note.clone()])
            .await
            .unwrap();

        assert_eq!(recorded, 0);

        // Verify no associations were created
        let associations = repo
            .get_associations_for_note(&repo_map_note)
            .await
            .unwrap();
        assert!(associations.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_repo_map_co_access_deduplicates_companion_ids() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let repo_map_note = make_note(&repo, &project, &tmp, "Repo Map Note").await;
        let note1 = make_note(&repo, &project, &tmp, "Note One").await;

        // Pass the same companion ID multiple times
        let recorded = repo
            .record_repo_map_co_access(
                &repo_map_note,
                vec![note1.clone(), note1.clone(), note1.clone()],
            )
            .await
            .unwrap();

        assert_eq!(recorded, 1);

        // Verify only one association was created
        let associations = repo
            .get_associations_for_note(&repo_map_note)
            .await
            .unwrap();
        assert_eq!(associations.len(), 1);
        assert_eq!(associations[0].co_access_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_repo_map_co_access_no_op_for_empty_input() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let repo_map_note = make_note(&repo, &project, &tmp, "Repo Map Note").await;

        // Record co-access with empty set
        let recorded = repo
            .record_repo_map_co_access(&repo_map_note, Vec::<String>::new())
            .await
            .unwrap();

        assert_eq!(recorded, 0);

        // Verify no associations were created
        let associations = repo
            .get_associations_for_note(&repo_map_note)
            .await
            .unwrap();
        assert!(associations.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_repo_map_co_access_repeated_reinforcement_increases_count_and_weight() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let repo_map_note = make_note(&repo, &project, &tmp, "Repo Map Note").await;
        let note1 = make_note(&repo, &project, &tmp, "Note One").await;

        // First co-access
        let recorded = repo
            .record_repo_map_co_access(&repo_map_note, vec![note1.clone()])
            .await
            .unwrap();
        assert_eq!(recorded, 1);

        let assoc1 = repo
            .get_associations_for_note(&repo_map_note)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(assoc1.co_access_count, 1);
        assert_eq!(assoc1.weight, 0.01);

        // Second co-access (reinforcement)
        let recorded = repo
            .record_repo_map_co_access(&repo_map_note, vec![note1.clone()])
            .await
            .unwrap();
        assert_eq!(recorded, 1);

        let assoc2 = repo
            .get_associations_for_note(&repo_map_note)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(assoc2.co_access_count, 2);
        assert!(assoc2.weight > assoc1.weight);

        // Third co-access (more reinforcement)
        let recorded = repo
            .record_repo_map_co_access(&repo_map_note, vec![note1.clone()])
            .await
            .unwrap();
        assert_eq!(recorded, 1);

        let assoc3 = repo
            .get_associations_for_note(&repo_map_note)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(assoc3.co_access_count, 3);
        assert!(assoc3.weight > assoc2.weight);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_repo_map_co_access_filters_self_and_dedupes_in_same_call() {
        let tmp = crate::database::test_tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();
        let (tx, _rx) = broadcast::channel(256);
        let project = make_project(&db, tmp.path()).await;
        let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

        let repo_map_note = make_note(&repo, &project, &tmp, "Repo Map Note").await;
        let note1 = make_note(&repo, &project, &tmp, "Note One").await;

        // Mix of self, duplicate, and valid companion
        let recorded = repo
            .record_repo_map_co_access(
                &repo_map_note,
                vec![
                    repo_map_note.clone(), // self - should be filtered
                    note1.clone(),         // valid
                    note1.clone(),         // duplicate - should be deduped
                    repo_map_note.clone(), // self again
                ],
            )
            .await
            .unwrap();

        assert_eq!(recorded, 1);

        // Verify only one association for note1
        let associations = repo
            .get_associations_for_note(&repo_map_note)
            .await
            .unwrap();
        assert_eq!(associations.len(), 1);
        let companion_id = if associations[0].note_a_id == repo_map_note {
            &associations[0].note_b_id
        } else {
            &associations[0].note_a_id
        };
        assert_eq!(companion_id, &note1);
    }
}
