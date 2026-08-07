//! L0 abstract regeneration state (migration 190, proposal u46i AC8).
//!
//! Owns every query behind the abstract backfill sweep. The control plane
//! orchestrates the sweep but must not speak SQL — `check-raw-sql-boundary.sh`
//! enforces that, and it is the reason this module exists rather than the
//! queries living next to the sweep loop.
//!
//! # Why staleness is not a text match
//!
//! The first cut decided staleness by matching the abstract's prose against a
//! literal prefix the prompt asks the model to emit. That made convergence
//! depend on model compliance: a note phrased differently stayed stale forever,
//! was re-selected from a fresh cursor on every run, and kept
//! [`AbstractVintageCoverage::is_converged`] structurally unreachable.
//!
//! Staleness is now "has not been regenerated under the current prompt
//! version", recorded per note, and a bounded attempt counter parks notes that
//! keep failing so they are *reported* rather than retried forever. Running a
//! completed sweep again is a no-op.

use serde::{Deserialize, Serialize};

use crate::database::Database;
use crate::error::DbResult as Result;

/// Default ceiling on regeneration attempts before a note is parked.
///
/// Deliberately small. An attempt costs a live LLM call; a note that has failed
/// three times is failing for a reason a fourth call will not fix, and the
/// operator needs to see it rather than pay for it repeatedly.
pub const DEFAULT_ABSTRACT_REGEN_ATTEMPT_LIMIT: i32 = 3;

/// How much of the corpus carries an abstract from the current L0 prompt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbstractVintageCoverage {
    /// Active notes in scope.
    pub total: i64,
    /// Notes regenerated under the current prompt version.
    pub current_vintage: i64,
    /// Notes not yet current whose attempt count is still under the bound —
    /// i.e. the work a sweep would still do.
    pub pending: i64,
    /// Notes not yet current that have exhausted the attempt bound. These are
    /// parked: no future sweep will select them, and they need a human.
    pub exhausted: i64,
}

impl AbstractVintageCoverage {
    /// Fraction of in-scope notes on the current prompt, in `0.0..=1.0`.
    pub fn current_ratio(&self) -> f64 {
        if self.total <= 0 {
            return 0.0;
        }
        self.current_vintage as f64 / self.total as f64
    }

    /// True when no note is still awaiting regeneration.
    ///
    /// Reachable by construction: `pending` excludes notes that have exhausted
    /// the attempt bound, so a corpus containing permanently failing notes
    /// still converges — it converges with a non-zero [`Self::exhausted`],
    /// which is the honest answer rather than an unreachable 100%.
    pub fn is_converged(&self) -> bool {
        self.total > 0 && self.pending == 0
    }

    /// True when convergence was reached with notes left parked.
    pub fn converged_with_failures(&self) -> bool {
        self.is_converged() && self.exhausted > 0
    }
}

/// One note awaiting regeneration. Carries the project id so the caller can
/// record the attempt without a second lookup per note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleAbstractNote {
    pub note_id: String,
    pub project_id: String,
}

/// One page of notes awaiting regeneration.
pub async fn select_stale_abstract_note_ids(
    db: &Database,
    prompt_version: &str,
    after_id: Option<&str>,
    limit: i64,
    note_types: &[String],
    attempt_limit: i32,
) -> Result<Vec<StaleAbstractNote>> {
    db.ensure_initialized().await?;
    let types: Vec<String> = note_types.to_vec();

    // `IS DISTINCT FROM` treats a missing row (NULL prompt_version) as stale,
    // so a note never attempted is selected without needing a seed row.
    let rows = sqlx::query_as::<_, (String, String)>(
        r#"SELECT n.id, n.project_id
           FROM notes n
           LEFT JOIN note_abstract_regenerations r ON r.note_id = n.id
           WHERE ($1::text IS NULL OR n.id > $1::text)
             AND (cardinality($2::text[]) = 0 OR n.note_type = ANY($2::text[]))
             AND n.status = 'active'
             AND r.prompt_version IS DISTINCT FROM $3::text
             AND (
                 r.attempt_version IS DISTINCT FROM $3::text
                 OR COALESCE(r.attempts, 0) < $4::int
             )
           ORDER BY n.id
           LIMIT $5"#,
    )
    .bind(after_id)
    .bind(&types)
    .bind(prompt_version)
    .bind(attempt_limit)
    .bind(limit.max(1))
    .fetch_all(db.pool())
    .await?;

    Ok(rows
        .into_iter()
        .map(|(note_id, project_id)| StaleAbstractNote {
            note_id,
            project_id,
        })
        .collect())
}

/// Count vintages for the same population the sweep acts on.
///
/// Scoped to `status = 'active'` and the same `note_types`, so a converged
/// sweep and a converged counter mean the same thing. A counter measuring a
/// wider population than the sweep can act on could never reach convergence.
pub async fn abstract_vintage_coverage(
    db: &Database,
    prompt_version: &str,
    note_types: &[String],
    attempt_limit: i32,
) -> Result<AbstractVintageCoverage> {
    db.ensure_initialized().await?;
    let types: Vec<String> = note_types.to_vec();

    let row: (i64, i64, i64, i64) = sqlx::query_as(
        r#"SELECT
               count(*) AS total,
               count(*) FILTER (
                   WHERE r.prompt_version IS NOT DISTINCT FROM $2::text
               ) AS current_vintage,
               count(*) FILTER (
                   WHERE r.prompt_version IS DISTINCT FROM $2::text
                     AND (
                         r.attempt_version IS DISTINCT FROM $2::text
                         OR COALESCE(r.attempts, 0) < $3::int
                     )
               ) AS pending,
               count(*) FILTER (
                   WHERE r.prompt_version IS DISTINCT FROM $2::text
                     AND r.attempt_version IS NOT DISTINCT FROM $2::text
                     AND COALESCE(r.attempts, 0) >= $3::int
               ) AS exhausted
           FROM notes n
           LEFT JOIN note_abstract_regenerations r ON r.note_id = n.id
           WHERE (cardinality($1::text[]) = 0 OR n.note_type = ANY($1::text[]))
             AND n.status = 'active'"#,
    )
    .bind(&types)
    .bind(prompt_version)
    .bind(attempt_limit)
    .fetch_one(db.pool())
    .await?;

    Ok(AbstractVintageCoverage {
        total: row.0,
        current_vintage: row.1,
        pending: row.2,
        exhausted: row.3,
    })
}

/// Record that `note_id` was handed to the generator.
///
/// Called at ENQUEUE time, not on completion: the bound must hold even if the
/// process dies before the note is generated, otherwise a crash loop reproduces
/// the unbounded retry this table exists to prevent.
pub async fn record_abstract_regeneration_attempt(
    db: &Database,
    project_id: &str,
    note_id: &str,
    prompt_version: &str,
) -> Result<()> {
    db.ensure_initialized().await?;
    // Attempts are counted per prompt version: when the version changes the
    // counter restarts at 1, so a note parked under an old prompt gets a fresh
    // budget under a new one. Otherwise fixing the prompt could never rescue
    // the notes the fix was written for.
    sqlx::query(
        r#"INSERT INTO note_abstract_regenerations
               (note_id, project_id, prompt_version, attempts, attempt_version, last_attempt_at)
           VALUES ($1, $2, NULL, 1, $3,
                   to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
           ON CONFLICT (note_id) DO UPDATE SET
               attempts = CASE
                   WHEN note_abstract_regenerations.attempt_version
                        IS NOT DISTINCT FROM EXCLUDED.attempt_version
                   THEN note_abstract_regenerations.attempts + 1
                   ELSE 1
               END,
               attempt_version = EXCLUDED.attempt_version,
               last_attempt_at = EXCLUDED.last_attempt_at"#,
    )
    .bind(note_id)
    .bind(project_id)
    .bind(prompt_version)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Record a successful regeneration under `prompt_version`.
///
/// Resets `attempts` so a later prompt version starts the note with a full
/// budget rather than inheriting an exhausted one from a previous vintage.
pub async fn record_abstract_regeneration_success(
    db: &Database,
    project_id: &str,
    note_id: &str,
    prompt_version: &str,
) -> Result<()> {
    db.ensure_initialized().await?;
    sqlx::query(
        r#"INSERT INTO note_abstract_regenerations
               (note_id, project_id, prompt_version, attempts, attempt_version,
                last_attempt_at, regenerated_at)
           VALUES ($1, $2, $3, 0, $3,
                   to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                   to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'))
           ON CONFLICT (note_id) DO UPDATE SET
               prompt_version = EXCLUDED.prompt_version,
               attempts = 0,
               attempt_version = EXCLUDED.prompt_version,
               last_attempt_at = EXCLUDED.last_attempt_at,
               regenerated_at = EXCLUDED.regenerated_at"#,
    )
    .bind(note_id)
    .bind(project_id)
    .bind(prompt_version)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Permalinks of notes parked at the attempt bound, for operator diagnostics.
pub async fn exhausted_abstract_regeneration_notes(
    db: &Database,
    prompt_version: &str,
    note_types: &[String],
    attempt_limit: i32,
    limit: i64,
) -> Result<Vec<String>> {
    db.ensure_initialized().await?;
    let types: Vec<String> = note_types.to_vec();
    let permalinks = sqlx::query_scalar::<_, String>(
        r#"SELECT n.permalink
           FROM notes n
           JOIN note_abstract_regenerations r ON r.note_id = n.id
           WHERE (cardinality($1::text[]) = 0 OR n.note_type = ANY($1::text[]))
             AND n.status = 'active'
             AND r.prompt_version IS DISTINCT FROM $2::text
             AND r.attempt_version IS NOT DISTINCT FROM $2::text
             AND r.attempts >= $3::int
           ORDER BY r.attempts DESC, n.permalink
           LIMIT $4"#,
    )
    .bind(&types)
    .bind(prompt_version)
    .bind(attempt_limit)
    .bind(limit.max(1))
    .fetch_all(db.pool())
    .await?;

    Ok(permalinks)
}

#[cfg(test)]
mod tests {
    use djinn_core::events::EventBus;

    use super::*;
    use crate::repositories::note::NoteRepository;
    use crate::repositories::test_support::make_project;

    const VERSION_A: &str = "l0-aaaaaaaaaaaaaaa";
    const VERSION_B: &str = "l0-bbbbbbbbbbbbbbb";
    const LIMIT: i32 = DEFAULT_ABSTRACT_REGEN_ATTEMPT_LIMIT;

    /// Project + note repository over an isolated ephemeral Postgres.
    async fn fixture() -> (Database, String, NoteRepository) {
        let db = Database::open_in_memory().unwrap();
        // `make_project` uses the path only to synthesize a unique name.
        let project = make_project(&db, std::path::Path::new("abstract-regen")).await;
        let repo = NoteRepository::new(db.clone(), EventBus::noop());
        (db, project.id, repo)
    }

    async fn seed_note(
        repo: &NoteRepository,
        project_id: &str,
        title: &str,
        note_type: &str,
    ) -> djinn_memory::Note {
        repo.create(project_id, title, "body", note_type, "[]")
            .await
            .unwrap()
    }

    async fn stale_ids(db: &Database, version: &str) -> Vec<String> {
        select_stale_abstract_note_ids(db, version, None, 100, &[], LIMIT)
            .await
            .unwrap()
            .into_iter()
            .map(|note| note.note_id)
            .collect()
    }

    /// Property 1: a note that has never been attempted is stale.
    ///
    /// There is no seed row for it, so this is really a statement about the
    /// LEFT JOIN: a missing regeneration row must read as "not current", not as
    /// "unknown, skip". Nothing back-fills rows for the ~10.7k existing notes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_note_never_attempted_is_selected_without_a_seed_row() {
        let (db, project_id, repo) = fixture().await;
        let note = seed_note(&repo, &project_id, "Never Attempted", "pitfall").await;

        let rows = select_stale_abstract_note_ids(&db, VERSION_A, None, 100, &[], LIMIT)
            .await
            .unwrap();

        assert_eq!(
            rows,
            vec![StaleAbstractNote {
                note_id: note.id,
                project_id: project_id.clone(),
            }],
            "a note with no regeneration row must be selected, and must carry \
             its project id so the caller can record the attempt"
        );
    }

    /// Property 2: idempotency. Once regenerated under the current prompt
    /// version a note leaves the stale set permanently.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_note_regenerated_under_the_current_prompt_version_is_not_selected() {
        let (db, project_id, repo) = fixture().await;
        let done = seed_note(&repo, &project_id, "Regenerated", "pitfall").await;
        let untouched = seed_note(&repo, &project_id, "Untouched", "pitfall").await;

        record_abstract_regeneration_success(&db, &project_id, &done.id, VERSION_A)
            .await
            .unwrap();

        assert_eq!(
            stale_ids(&db, VERSION_A).await,
            vec![untouched.id],
            "a note stamped with the current version must not be re-selected"
        );
    }

    /// Property 3: the anti-infinite-spend bound. A note is selectable at
    /// `attempt_limit - 1` attempts and drops out at exactly `attempt_limit`.
    ///
    /// This is the property whose absence made the first cut re-pay for the
    /// same non-compliant notes on every run, forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_note_that_reached_the_attempt_limit_is_not_selected() {
        let (db, project_id, repo) = fixture().await;
        let note = seed_note(&repo, &project_id, "Keeps Failing", "pitfall").await;

        for attempt in 1..LIMIT {
            record_abstract_regeneration_attempt(&db, &project_id, &note.id, VERSION_A)
                .await
                .unwrap();
            assert_eq!(
                stale_ids(&db, VERSION_A).await,
                vec![note.id.clone()],
                "still selectable after {attempt} of {LIMIT} attempts"
            );
        }

        record_abstract_regeneration_attempt(&db, &project_id, &note.id, VERSION_A)
            .await
            .unwrap();

        assert!(
            stale_ids(&db, VERSION_A).await.is_empty(),
            "a note that has consumed all {LIMIT} attempts must be parked, not retried"
        );
    }

    /// Property 6: a prompt-version change re-stales the corpus AND hands every
    /// note a fresh attempt budget.
    ///
    /// Without the budget reset, a prompt fix could never rescue the very notes
    /// it was written to rescue: they would already be parked under the old
    /// count. That is what `attempt_version` exists for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_prompt_version_change_restales_the_corpus_and_restores_the_attempt_budget() {
        let (db, project_id, repo) = fixture().await;
        let parked = seed_note(&repo, &project_id, "Parked Under A", "pitfall").await;
        let succeeded = seed_note(&repo, &project_id, "Succeeded Under A", "pitfall").await;

        for _ in 0..LIMIT {
            record_abstract_regeneration_attempt(&db, &project_id, &parked.id, VERSION_A)
                .await
                .unwrap();
        }
        record_abstract_regeneration_success(&db, &project_id, &succeeded.id, VERSION_A)
            .await
            .unwrap();

        assert!(
            stale_ids(&db, VERSION_A).await.is_empty(),
            "under version A the corpus is settled: one current, one parked"
        );

        let mut under_b = stale_ids(&db, VERSION_B).await;
        under_b.sort();
        let mut expected = vec![parked.id.clone(), succeeded.id];
        expected.sort();
        assert_eq!(
            under_b, expected,
            "a new prompt version re-stales every note, including the parked one"
        );

        // And the budget really was restored, not merely the staleness flag:
        // the parked note survives `attempt_limit - 1` fresh attempts under B.
        for _ in 1..LIMIT {
            record_abstract_regeneration_attempt(&db, &project_id, &parked.id, VERSION_B)
                .await
                .unwrap();
        }
        assert!(
            stale_ids(&db, VERSION_B).await.contains(&parked.id),
            "the previously parked note must get a full budget under the new version"
        );
    }

    /// Property 7: `total` partitions exactly into
    /// `current_vintage + pending + exhausted` over a mixed corpus.
    ///
    /// If it did not, "converged" and "still has work" could both be false at
    /// once and the operator would have no way to tell which.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn total_partitions_into_current_vintage_pending_and_exhausted() {
        let (db, project_id, repo) = fixture().await;

        let current = seed_note(&repo, &project_id, "Current", "pitfall").await;
        seed_note(&repo, &project_id, "Never Attempted", "pitfall").await;
        let half_tried = seed_note(&repo, &project_id, "Half Tried", "pitfall").await;
        let parked = seed_note(&repo, &project_id, "Parked", "pitfall").await;
        // Out of scope for the `pitfall` sweep; must not appear in any bucket.
        seed_note(&repo, &project_id, "Other Type", "reference").await;

        record_abstract_regeneration_success(&db, &project_id, &current.id, VERSION_A)
            .await
            .unwrap();
        record_abstract_regeneration_attempt(&db, &project_id, &half_tried.id, VERSION_A)
            .await
            .unwrap();
        for _ in 0..LIMIT {
            record_abstract_regeneration_attempt(&db, &project_id, &parked.id, VERSION_A)
                .await
                .unwrap();
        }

        let coverage = abstract_vintage_coverage(&db, VERSION_A, &["pitfall".to_string()], LIMIT)
            .await
            .unwrap();

        assert_eq!(coverage.total, 4, "only in-scope note types are counted");
        assert_eq!(coverage.current_vintage, 1);
        assert_eq!(
            coverage.pending, 2,
            "never-attempted and half-tried are the work a sweep would still do"
        );
        assert_eq!(coverage.exhausted, 1);
        assert_eq!(
            coverage.current_vintage + coverage.pending + coverage.exhausted,
            coverage.total,
            "the three buckets must partition the corpus with no note in two of \
             them and none in neither"
        );
        assert!(!coverage.is_converged(), "two notes are still pending");
        assert!((coverage.current_ratio() - 0.25).abs() < 1e-9);

        // `pending` is exactly what selection would return.
        let pending_now = select_stale_abstract_note_ids(
            &db,
            VERSION_A,
            None,
            100,
            &["pitfall".to_string()],
            LIMIT,
        )
        .await
        .unwrap();
        assert_eq!(pending_now.len() as i64, coverage.pending);
    }

    /// Property 9: parked notes are reported to the operator by permalink.
    ///
    /// Parking a note is only acceptable because the note becomes visible; an
    /// invisible park is silent data loss.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exhausted_abstract_regeneration_notes_returns_the_parked_permalinks() {
        let (db, project_id, repo) = fixture().await;

        let parked_one = seed_note(&repo, &project_id, "Parked One", "pitfall").await;
        let parked_two = seed_note(&repo, &project_id, "Parked Two", "pitfall").await;
        let pending = seed_note(&repo, &project_id, "Still Pending", "pitfall").await;
        let current = seed_note(&repo, &project_id, "Already Current", "pitfall").await;

        for note in [&parked_one, &parked_two] {
            for _ in 0..LIMIT {
                record_abstract_regeneration_attempt(&db, &project_id, &note.id, VERSION_A)
                    .await
                    .unwrap();
            }
        }
        record_abstract_regeneration_attempt(&db, &project_id, &pending.id, VERSION_A)
            .await
            .unwrap();
        record_abstract_regeneration_success(&db, &project_id, &current.id, VERSION_A)
            .await
            .unwrap();

        let mut reported = exhausted_abstract_regeneration_notes(
            &db,
            VERSION_A,
            &["pitfall".to_string()],
            LIMIT,
            100,
        )
        .await
        .unwrap();
        reported.sort();

        let mut expected = vec![parked_one.permalink, parked_two.permalink];
        expected.sort();
        assert_eq!(
            reported, expected,
            "only notes at the attempt bound are reported: not the pending one, \
             not the current one"
        );
    }
}
