//! Bounded, subtractive consolidation lifecycle (proposal `t5rn`).
//!
//! This module owns the four mechanisms the old consolidation path was missing:
//!
//! * **Exact source eligibility** — canonical notes are identified by their
//!   *immutable* creation revision attributed to the `consolidation` subsystem,
//!   never by a mutable tag, title, permalink, content, or edge shape. A generic
//!   tag/content update that strips the display tag therefore cannot make a
//!   canonical eligible again.
//! * **A resumable provenance backfill** driven only by trusted immutable
//!   revision session provenance.
//! * **Bounded clustering** — at most 200 inputs per partition, one set-based
//!   directed score-matrix query (no per-note or per-pair SQL), memoized
//!   complete-link admission bounded by `C(200, 2) = 19_900` unordered
//!   comparisons, and clusters of 3–8 sources.
//! * **One atomic canonical transaction** — canonical note, its immutable
//!   consolidation-attributed creation revision carrying a unique attempt
//!   identity, the display tag, summary fields, provenance rows, `supersedes`
//!   edges, and every source's `active → superseded` revision commit together
//!   or not at all.
//!
//! # Why source status is never a success witness
//!
//! A source can be retired by write-time dedup or by a *different* consolidation
//! set. An ambiguous retry therefore looks up the immutable creation revision by
//! `consolidation_attempt_id` and reports success only when exactly one canonical
//! exists whose partition, canonical body digest, and exact `supersedes`
//! endpoint set equal the request. All-superseded or mixed source sets with no
//! exact match are a conflict that creates nothing.

use std::collections::{BTreeSet, HashMap, HashSet};

use djinn_memory::ConsolidationNote;

use super::consolidation::{
    NoteConsolidationRepository, is_consolidation_eligible_note_type, sanitize_fts5_query,
};
use super::{
    CONFIDENCE_CEILING, CONFIDENCE_FLOOR, NoteRevisionReason, folder_for_type,
    index_links_for_note, permalink_for, resolve_links_for_note,
};
use crate::error::{DbError as Error, DbResult as Result};
use crate::note_hash::note_content_hash;

// ── Normative bounds ─────────────────────────────────────────────────────────

/// Maximum eligible inputs considered for one `(session, project, note_type)`
/// partition in one run. Overflow is reported and deferred to later runs; it is
/// never randomly sampled.
pub const CONSOLIDATION_MAX_PARTITION_INPUTS: usize = 200;

/// Initial configurable complete-link admission threshold.
pub const CONSOLIDATION_DEFAULT_SCORE_THRESHOLD: f64 = 0.15;

/// Smallest source count that makes a cluster qualifying.
pub const CONSOLIDATION_MIN_CLUSTER_SOURCES: usize = 3;

/// Largest source count admitted into one canonical.
pub const CONSOLIDATION_MAX_CLUSTER_SOURCES: usize = 8;

/// `C(200, 2)` — the hard bound on unordered admission comparisons per
/// partition per run implied by the 200-input cap.
pub const CONSOLIDATION_MAX_ADMISSION_COMPARISONS: usize = 19_900;

/// Resumable backfill batch size.
pub const CONSOLIDATION_BACKFILL_BATCH_ROWS: i64 = 1_000;

/// Reserved display/indexing tag written on a canonical. Deliberately *not*
/// load-bearing: source exclusion uses immutable creation attribution.
pub const CONSOLIDATION_CANONICAL_TAG: &str = "consolidation-canonical";

/// Version prefix mixed into `consolidation_attempt_id` so a future change to
/// the identity recipe cannot collide with attempts committed under this one.
pub const CONSOLIDATION_ATTEMPT_VERSION: &str = "t5rn.consolidation-attempt.v1";

/// Note types allowed into consolidation.
pub const CONSOLIDATION_ELIGIBLE_NOTE_TYPES: [&str; 3] = ["case", "pattern", "pitfall"];

// ── Partition and selection ──────────────────────────────────────────────────

/// The exactly-one grouping key an enabled run may address.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConsolidationPartitionKey {
    pub project_id: String,
    pub session_id: String,
    pub note_type: String,
}

impl ConsolidationPartitionKey {
    /// Reject missing, blank, or non-eligible keys before any synthesis.
    pub fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("project_id", self.project_id.as_str()),
            ("session_id", self.session_id.as_str()),
            ("note_type", self.note_type.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(Error::InvalidData(format!(
                    "consolidation partition {field} must be exactly one non-blank value"
                )));
            }
            if value.trim() != value {
                return Err(Error::InvalidData(format!(
                    "consolidation partition {field} must not carry surrounding whitespace"
                )));
            }
            // A wildcard or multi-valued key would widen the partition, which is
            // explicitly out of scope for the first production consolidation.
            if value.contains(',') || value.contains('*') || value.contains('%') {
                return Err(Error::InvalidData(format!(
                    "consolidation partition {field} must be exactly one literal value"
                )));
            }
        }
        if !is_consolidation_eligible_note_type(&self.note_type) {
            return Err(Error::InvalidData(format!(
                "consolidation partition note_type must be one of {:?}",
                CONSOLIDATION_ELIGIBLE_NOTE_TYPES
            )));
        }
        Ok(())
    }
}

/// Deterministically capped eligible inputs for one partition.
#[derive(Debug, Clone, PartialEq)]
pub struct EligibleSourceSelection {
    /// At most [`CONSOLIDATION_MAX_PARTITION_INPUTS`] notes, ordered
    /// `updated_at DESC, id ASC`.
    pub notes: Vec<ConsolidationNote>,
    /// Eligible notes beyond the cap, deferred to later runs.
    pub overflow_count: usize,
}

#[derive(Debug, sqlx::FromRow)]
struct EligibleSourceRow {
    id: String,
    project_id: String,
    permalink: String,
    title: String,
    note_type: String,
    folder: String,
    scope_paths: String,
    content: String,
    abstract_: Option<String>,
    overview: Option<String>,
    confidence: f64,
    eligible_total: i64,
}

/// One directed `(seed, candidate)` score produced by the set-based matrix
/// query. The score expression is the *existing* `ts_rank` expression; this
/// changes only how many round trips compute it.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct DirectedScoreRow {
    pub seed_id: String,
    pub candidate_id: String,
    pub score: f64,
}

/// One qualifying complete-link cluster.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundedCluster {
    /// Sorted ascending — the canonical transaction's lock order.
    pub source_note_ids: Vec<String>,
    /// Cluster members in `(permalink, id)` order.
    pub ordered_notes: Vec<ConsolidationNote>,
}

/// The full deterministic clustering outcome for one partition run.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundedClusteringOutcome {
    pub clusters: Vec<BoundedCluster>,
    /// Distinct unordered admission comparisons actually evaluated. Bounded by
    /// [`CONSOLIDATION_MAX_ADMISSION_COMPARISONS`] because every pair is
    /// evaluated at most once.
    pub admission_comparisons: usize,
    pub score_matrix_rows: usize,
    pub input_count: usize,
    pub overflow_count: usize,
}

// ── Backfill ─────────────────────────────────────────────────────────────────

/// Machine-readable outcome of the resumable one-time provenance backfill.
///
/// Every number is reported, never guessed: notes without trusted immutable
/// session provenance stay ineligible instead of being matched by timestamp,
/// title, or content.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProvenanceBackfillReport {
    pub scanned_note_count: i64,
    pub seeded_provenance_row_count: i64,
    pub skipped_without_provenance: i64,
    pub skipped_canonical_attribution: i64,
    pub skipped_project_mismatch: i64,
    pub batches_processed: i64,
    pub completed: bool,
    pub last_note_id: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct BackfillStateRow {
    last_note_id: Option<String>,
    completed: bool,
    scanned_note_count: i64,
    seeded_provenance_row_count: i64,
    skipped_without_provenance: i64,
    skipped_canonical_attribution: i64,
    skipped_project_mismatch: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct BatchClassification {
    canonical_count: i64,
    without_provenance_count: i64,
    project_mismatch_count: i64,
}

// ── Atomic canonical transaction ─────────────────────────────────────────────

/// Request for the single subtractive state transition.
#[derive(Debug, Clone)]
pub struct CommitConsolidationCanonical<'a> {
    pub partition: &'a ConsolidationPartitionKey,
    /// Requested sources. Sorted ascending internally before locking.
    pub source_note_ids: &'a [String],
    pub title: &'a str,
    pub content: &'a str,
    pub abstract_: Option<&'a str>,
    pub overview: Option<&'a str>,
    pub confidence: f64,
    pub scope_paths: &'a str,
    pub reason: NoteRevisionReason,
}

/// The exact committed effects of one canonical transaction. This is what a
/// run report's `write_result` is built from.
#[derive(Debug, Clone, PartialEq)]
pub struct CommittedConsolidationCanonical {
    pub canonical_note_id: String,
    pub consolidation_attempt_id: String,
    pub canonical_body_digest: String,
    pub creation_revision_id: String,
    pub canonical_provenance_session_ids: Vec<String>,
    /// `supersedes` edge endpoints, sorted ascending.
    pub supersedes_source_note_ids: Vec<String>,
    /// Final `(note_id, status)` for every requested source, sorted by id.
    pub final_source_statuses: Vec<(String, String)>,
}

/// Why an attempt could not be committed and must not claim completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsolidationConflictReason {
    /// One or more locked sources no longer satisfy the exact eligibility key
    /// (retired by write-time dedup, by a different consolidation set, archived,
    /// retyped, or promoted to a canonical).
    SourceNotEligible,
    /// A locked source does not belong to the requested partition.
    PartitionMismatch,
    /// A canonical carrying this attempt identity exists but its partition,
    /// canonical body digest, or `supersedes` endpoint set differs from the
    /// request. This is an invariant violation, never a success.
    AttemptIdentityMismatch,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationConflict {
    pub reason: ConsolidationConflictReason,
    pub detail: String,
    /// Observed `(note_id, status)` for every requested source, sorted by id.
    /// Reported for operators; never accepted as evidence of completion.
    pub observed_source_statuses: Vec<(String, String)>,
}

/// The three terminal outcomes of a canonical attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum ConsolidationCommitOutcome {
    /// This call performed the six writes.
    Committed(CommittedConsolidationCanonical),
    /// An earlier call already committed *this exact* attempt: the attempt-ID
    /// canonical exists and its partition, digest, and endpoint set match.
    AlreadyCommitted(CommittedConsolidationCanonical),
    /// Nothing was created and nothing was mutated.
    Conflict(ConsolidationConflict),
}

/// Write boundaries inside the canonical transaction, used only by the
/// fault-injection fixtures that prove rollback leaves no partial effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsolidationWriteBoundary {
    CanonicalNoteInsert = 1,
    CanonicalCreationRevision = 2,
    CanonicalSummaryFields = 3,
    CanonicalProvenanceRows = 4,
    SupersedesEdges = 5,
    SourceStatusTransition = 6,
}

impl ConsolidationWriteBoundary {
    pub const ALL: [Self; 6] = [
        Self::CanonicalNoteInsert,
        Self::CanonicalCreationRevision,
        Self::CanonicalSummaryFields,
        Self::CanonicalProvenanceRows,
        Self::SupersedesEdges,
        Self::SourceStatusTransition,
    ];

    pub(super) fn code(self) -> u8 {
        self as u8
    }
}

/// Immutable retry witness located by `consolidation_attempt_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConsolidationAttemptWitness {
    pub canonical_note_id: String,
    pub creation_revision_id: String,
    pub project_id: String,
    pub note_type: String,
    pub session_id: Option<String>,
    pub canonical_body_digest: String,
    /// `supersedes` endpoints of the located canonical, sorted ascending.
    pub supersedes_source_note_ids: Vec<String>,
    pub canonical_provenance_session_ids: Vec<String>,
}

#[derive(Debug, sqlx::FromRow)]
struct AttemptWitnessRow {
    revision_id: String,
    note_id: String,
    project_id: String,
    note_type: String,
    session_id: Option<String>,
    content: String,
}

#[derive(Debug, sqlx::FromRow)]
struct LockedSourceRow {
    id: String,
    project_id: String,
    note_type: String,
    storage: String,
    status: String,
    content: String,
    confidence: f64,
    in_partition_session: bool,
    consolidation_attributed: bool,
}

// ── Passive partition pressure (T6) ──────────────────────────────────────────

/// One passive `(project_id, note_type)` pressure reading.
///
/// Captured once per housekeeping sweep from a single statement snapshot. This
/// is report-only: no write-path count, prompt policy, task, deletion, or
/// automatic actuator is derived from it.
#[derive(Debug, Clone, PartialEq)]
pub struct PartitionPressureMetric {
    pub project_id: String,
    pub note_type: String,
    /// Active DB-backed non-canonical notes of the type allowed into retrieval.
    pub eligible_notes: i64,
    /// Configured maximum notes of that type available to one context build.
    pub injectable_slots: i64,
    /// `eligible_notes / injectable_slots`, present only when slots are
    /// positive.
    pub oversubscription_ratio: Option<f64>,
    /// Set when `injectable_slots == 0`; the ratio is then absent.
    pub unbounded_pressure: bool,
}

#[derive(Debug, sqlx::FromRow)]
struct PartitionPressureRow {
    project_id: String,
    note_type: String,
    eligible_notes: i64,
}

impl NoteConsolidationRepository {
    // ── T1: eligible sources ────────────────────────────────────────────────

    /// Select the exact eligible source set for one partition, capped at
    /// [`CONSOLIDATION_MAX_PARTITION_INPUTS`] and ordered
    /// `updated_at DESC, id ASC`.
    ///
    /// The eligibility key is exactly the one T3 revalidates under lock:
    /// session provenance, same project as the session, eligible note type,
    /// `storage = 'db'`, `status = 'active'`, and **no immutable creation
    /// revision with trusted `subsystem = 'consolidation'` attribution**. The
    /// last predicate is what makes a canonical permanently ineligible even
    /// after a generic tag/content update strips its display tag.
    ///
    /// One round trip: the eligible total is computed by a window function on
    /// the same scan, so overflow is reported without a second query.
    pub async fn select_eligible_partition_sources(
        &self,
        partition: &ConsolidationPartitionKey,
    ) -> Result<EligibleSourceSelection> {
        partition.validate()?;
        self.db.ensure_initialized().await?;

        let rows: Vec<EligibleSourceRow> = sqlx::query_as(
            r#"WITH eligible AS (
                 SELECT n.id, n.project_id, n.permalink, n.title, n.note_type, n.folder,
                        n.scope_paths::text AS scope_paths, n.content,
                        n.abstract AS abstract_, n.overview, n.confidence, n.updated_at
                 FROM notes n
                 JOIN consolidated_note_provenance cnp ON cnp.note_id = n.id
                 JOIN sessions s ON s.id = cnp.session_id
                 WHERE cnp.session_id = $1
                   AND s.project_id = $2
                   AND n.project_id = $2
                   AND n.note_type = $3
                   AND n.storage = 'db'
                   AND n.status = 'active'
                   AND NOT EXISTS (
                         SELECT 1
                         FROM note_revision_events nre
                         WHERE nre.note_id = n.id
                           AND nre.event_kind = 'created'
                           AND nre.actor_kind = 'system'
                           AND nre.subsystem = 'consolidation'
                       )
               )
               SELECT id, project_id, permalink, title, note_type, folder, scope_paths,
                      content, abstract_, overview, confidence,
                      COUNT(*) OVER () AS eligible_total
               FROM eligible
               ORDER BY updated_at DESC, id ASC
               LIMIT $4"#,
        )
        .bind(&partition.session_id)
        .bind(&partition.project_id)
        .bind(&partition.note_type)
        .bind(CONSOLIDATION_MAX_PARTITION_INPUTS as i64)
        .fetch_all(self.db.pool())
        .await?;

        let eligible_total = rows.first().map_or(0i64, |row| row.eligible_total);
        let notes = rows
            .into_iter()
            .map(|row| ConsolidationNote {
                id: row.id,
                project_id: row.project_id,
                permalink: row.permalink,
                title: row.title,
                note_type: row.note_type,
                folder: row.folder,
                scope_paths: row.scope_paths,
                content: row.content,
                abstract_: row.abstract_,
                overview: row.overview,
                confidence: row.confidence,
            })
            .collect::<Vec<_>>();
        let overflow_count = (eligible_total as usize).saturating_sub(notes.len());

        Ok(EligibleSourceSelection {
            notes,
            overflow_count,
        })
    }

    /// True when the note's *immutable* creation revision was written by the
    /// consolidation subsystem. This is the authoritative canonical identity.
    pub async fn is_consolidation_canonical(&self, note_id: &str) -> Result<bool> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar::<_, bool>(
            r#"SELECT EXISTS (
                 SELECT 1 FROM note_revision_events
                 WHERE note_id = $1
                   AND event_kind = 'created'
                   AND actor_kind = 'system'
                   AND subsystem = 'consolidation'
               )"#,
        )
        .bind(note_id)
        .fetch_one(self.db.pool())
        .await?)
    }

    // ── T1: resumable backfill ──────────────────────────────────────────────

    /// Seed `consolidated_note_provenance` from trusted immutable revision
    /// session provenance, resumably, in batches of
    /// [`CONSOLIDATION_BACKFILL_BATCH_ROWS`].
    ///
    /// * Only `system`/`extraction`-attributed revisions with a non-null
    ///   `session_id` are trusted. Nothing is inferred from timestamps, titles,
    ///   or content.
    /// * The session must belong to the note's project.
    /// * The destination note must satisfy the exact source eligibility key.
    /// * The insert is `ON CONFLICT DO NOTHING` on the same
    ///   `(note_id, session_id)` uniqueness live writes use, so it is idempotent.
    ///
    /// `scope_key` names the resume watermark row. `max_batches = None` runs to
    /// completion; `Some(n)` stops after `n` batches and leaves a resumable
    /// watermark. Re-invoking a completed scope returns the stored report
    /// without rescanning.
    pub async fn run_provenance_backfill(
        &self,
        scope_key: &str,
        max_batches: Option<usize>,
    ) -> Result<ProvenanceBackfillReport> {
        self.db.ensure_initialized().await?;
        if scope_key.trim().is_empty() {
            return Err(Error::InvalidData(
                "provenance backfill scope key must not be blank".to_owned(),
            ));
        }

        let state: Option<BackfillStateRow> = sqlx::query_as(
            r#"SELECT last_note_id, completed, scanned_note_count, seeded_provenance_row_count,
                      skipped_without_provenance, skipped_canonical_attribution,
                      skipped_project_mismatch
               FROM consolidation_provenance_backfill_state
               WHERE scope_key = $1"#,
        )
        .bind(scope_key)
        .fetch_optional(self.db.pool())
        .await?;

        let mut report = match &state {
            Some(row) => ProvenanceBackfillReport {
                scanned_note_count: row.scanned_note_count,
                seeded_provenance_row_count: row.seeded_provenance_row_count,
                skipped_without_provenance: row.skipped_without_provenance,
                skipped_canonical_attribution: row.skipped_canonical_attribution,
                skipped_project_mismatch: row.skipped_project_mismatch,
                batches_processed: 0,
                completed: row.completed,
                last_note_id: row.last_note_id.clone(),
            },
            None => ProvenanceBackfillReport::default(),
        };
        if report.completed {
            return Ok(report);
        }

        let mut cursor = report.last_note_id.clone();
        let mut batches = 0usize;
        loop {
            if max_batches.is_some_and(|limit| batches >= limit) {
                break;
            }
            let batch_ids: Vec<String> = sqlx::query_scalar(
                r#"SELECT id
                   FROM notes
                   WHERE storage = 'db'
                     AND status = 'active'
                     AND note_type = ANY($1::text[])
                     AND ($2::text IS NULL OR id > $2::text)
                   ORDER BY id ASC
                   LIMIT $3"#,
            )
            .bind(eligible_note_type_array())
            .bind(cursor.as_deref())
            .bind(CONSOLIDATION_BACKFILL_BATCH_ROWS)
            .fetch_all(self.db.pool())
            .await?;

            if batch_ids.is_empty() {
                report.completed = true;
                break;
            }

            // Seed every trusted, same-project, eligible pairing in this batch.
            let seeded = sqlx::query(
                r#"INSERT INTO consolidated_note_provenance (note_id, session_id, created_at)
                   SELECT DISTINCT nre.note_id, nre.session_id,
                          to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                   FROM note_revision_events nre
                   JOIN notes n ON n.id = nre.note_id
                   JOIN sessions s ON s.id = nre.session_id AND s.project_id = n.project_id
                   WHERE nre.note_id = ANY($1::text[])
                     AND nre.session_id IS NOT NULL
                     AND nre.actor_kind = 'system'
                     AND nre.subsystem = 'extraction'
                     AND n.storage = 'db'
                     AND n.status = 'active'
                     AND n.note_type = ANY($2::text[])
                     AND NOT EXISTS (
                           SELECT 1 FROM note_revision_events canonical
                           WHERE canonical.note_id = n.id
                             AND canonical.event_kind = 'created'
                             AND canonical.actor_kind = 'system'
                             AND canonical.subsystem = 'consolidation'
                         )
                   ON CONFLICT (note_id, session_id) DO NOTHING"#,
            )
            .bind(&batch_ids)
            .bind(eligible_note_type_array())
            .execute(self.db.pool())
            .await?
            .rows_affected() as i64;

            // Classify what this batch could not seed, so the run reports rather
            // than guesses.
            let classification: BatchClassification = sqlx::query_as(
                r#"SELECT
                     COUNT(*) FILTER (WHERE canonical) AS canonical_count,
                     COUNT(*) FILTER (WHERE NOT canonical AND NOT has_trusted_session)
                       AS without_provenance_count,
                     COUNT(*) FILTER (
                         WHERE NOT canonical AND has_trusted_session AND NOT has_same_project_session
                     ) AS project_mismatch_count
                   FROM (
                     SELECT
                       EXISTS (
                         SELECT 1 FROM note_revision_events canonical
                         WHERE canonical.note_id = n.id
                           AND canonical.event_kind = 'created'
                           AND canonical.actor_kind = 'system'
                           AND canonical.subsystem = 'consolidation'
                       ) AS canonical,
                       EXISTS (
                         SELECT 1 FROM note_revision_events r
                         WHERE r.note_id = n.id
                           AND r.session_id IS NOT NULL
                           AND r.actor_kind = 'system'
                           AND r.subsystem = 'extraction'
                       ) AS has_trusted_session,
                       EXISTS (
                         SELECT 1 FROM note_revision_events r
                         JOIN sessions s ON s.id = r.session_id AND s.project_id = n.project_id
                         WHERE r.note_id = n.id
                           AND r.actor_kind = 'system'
                           AND r.subsystem = 'extraction'
                       ) AS has_same_project_session
                     FROM notes n
                     WHERE n.id = ANY($1::text[])
                   ) classified"#,
            )
            .bind(&batch_ids)
            .fetch_one(self.db.pool())
            .await?;

            report.scanned_note_count += batch_ids.len() as i64;
            report.seeded_provenance_row_count += seeded;
            report.skipped_canonical_attribution += classification.canonical_count;
            report.skipped_without_provenance += classification.without_provenance_count;
            report.skipped_project_mismatch += classification.project_mismatch_count;
            report.batches_processed += 1;
            cursor = batch_ids.last().cloned();
            report.last_note_id = cursor.clone();
            batches += 1;

            self.persist_backfill_state(scope_key, &report).await?;

            if (batch_ids.len() as i64) < CONSOLIDATION_BACKFILL_BATCH_ROWS {
                report.completed = true;
                break;
            }
        }

        self.persist_backfill_state(scope_key, &report).await?;
        Ok(report)
    }

    async fn persist_backfill_state(
        &self,
        scope_key: &str,
        report: &ProvenanceBackfillReport,
    ) -> Result<()> {
        sqlx::query(
            r#"INSERT INTO consolidation_provenance_backfill_state (
                   scope_key, last_note_id, completed, scanned_note_count,
                   seeded_provenance_row_count, skipped_without_provenance,
                   skipped_canonical_attribution, skipped_project_mismatch, updated_at
               ) VALUES (
                   $1, $2, $3, $4, $5, $6, $7, $8,
                   to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               )
               ON CONFLICT (scope_key) DO UPDATE SET
                   last_note_id = EXCLUDED.last_note_id,
                   completed = EXCLUDED.completed,
                   scanned_note_count = EXCLUDED.scanned_note_count,
                   seeded_provenance_row_count = EXCLUDED.seeded_provenance_row_count,
                   skipped_without_provenance = EXCLUDED.skipped_without_provenance,
                   skipped_canonical_attribution = EXCLUDED.skipped_canonical_attribution,
                   skipped_project_mismatch = EXCLUDED.skipped_project_mismatch,
                   updated_at = EXCLUDED.updated_at"#,
        )
        .bind(scope_key)
        .bind(report.last_note_id.as_deref())
        .bind(report.completed)
        .bind(report.scanned_note_count)
        .bind(report.seeded_provenance_row_count)
        .bind(report.skipped_without_provenance)
        .bind(report.skipped_canonical_attribution)
        .bind(report.skipped_project_mismatch)
        .execute(self.db.pool())
        .await?;
        Ok(())
    }

    // ── T2/T5: one set-based directed score matrix ──────────────────────────

    /// Materialize the partition's directed duplicate-score matrix in **one**
    /// SQL round trip.
    ///
    /// The score expression is byte-for-byte the one the previous per-seed
    /// `dedup_candidates_for_group` call issued: the same
    /// `sanitize_fts5_query` normalization feeds the same
    /// `websearch_to_tsquery('english', …)`, and the same
    /// `ts_rank(search_vector, …)` is computed against candidates in the same
    /// project/folder/type/storage scope. Only the round-trip fan-out changes:
    /// what used to be one query per seed is now one query per partition.
    ///
    /// Returns `(seed_id, candidate_id, score)` rows for consumption by
    /// [`build_bounded_clusters`], ordered deterministically.
    pub async fn directed_score_matrix(
        &self,
        project_id: &str,
        folder: &str,
        note_type: &str,
        notes: &[ConsolidationNote],
        score_threshold: f64,
    ) -> Result<Vec<DirectedScoreRow>> {
        self.db.ensure_initialized().await?;
        if notes.len() < 2 {
            return Ok(Vec::new());
        }

        let mut seed_ids = Vec::with_capacity(notes.len());
        let mut seed_queries = Vec::with_capacity(notes.len());
        for note in notes {
            // Seeds whose text sanitizes to nothing produced no candidates in
            // the per-seed path either; they are dropped from the relation
            // rather than turned into a separate round trip.
            if let Some(query_text) = sanitize_fts5_query(&build_partition_query_text(note)) {
                seed_ids.push(note.id.clone());
                seed_queries.push(query_text);
            }
        }
        if seed_ids.is_empty() {
            return Ok(Vec::new());
        }
        let candidate_ids = notes.iter().map(|note| note.id.clone()).collect::<Vec<_>>();

        // The consolidation sweep is latency-insensitive; hold the shared
        // background-search permit so a fan-out of partitions cannot starve the
        // pool. Unchanged from the per-seed path — except this now covers one
        // query instead of N.
        let _permit = self.db.background_search_permit().await;

        sqlx::query_as::<_, DirectedScoreRow>(
            r#"WITH seeds AS (
                 SELECT seed_id, query_text
                 FROM UNNEST($1::text[], $2::text[]) AS t(seed_id, query_text)
               ),
               scoped AS (
                 SELECT n.id, n.search_vector
                 FROM notes n
                 WHERE n.id = ANY($3::text[])
                   AND n.project_id = $4
                   AND n.folder = $5
                   AND n.note_type = $6
                   AND n.storage = 'db'
               ),
               ranked AS (
                 SELECT s.seed_id,
                        c.id AS candidate_id,
                        ts_rank(c.search_vector,
                                websearch_to_tsquery('english', s.query_text))::float8 AS score
                 FROM seeds s
                 JOIN scoped c ON c.id <> s.seed_id
                 WHERE c.search_vector @@ websearch_to_tsquery('english', s.query_text)
               )
               SELECT seed_id, candidate_id, score
               FROM ranked
               WHERE score > $7
               ORDER BY seed_id ASC, candidate_id ASC"#,
        )
        .bind(seed_ids)
        .bind(seed_queries)
        .bind(candidate_ids)
        .bind(project_id)
        .bind(folder)
        .bind(note_type)
        .bind(score_threshold)
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }

    /// Select, score, and cluster one partition using exactly **one** score
    /// query on top of the single eligibility query.
    pub async fn bounded_clusters_for_partition(
        &self,
        partition: &ConsolidationPartitionKey,
        score_threshold: f64,
    ) -> Result<BoundedClusteringOutcome> {
        let selection = self.select_eligible_partition_sources(partition).await?;
        let folder = folder_for_type(&partition.note_type).to_owned();
        let scores = self
            .directed_score_matrix(
                &partition.project_id,
                &folder,
                &partition.note_type,
                &selection.notes,
                score_threshold,
            )
            .await?;
        let (clusters, admission_comparisons) =
            build_bounded_clusters(&selection.notes, &scores, score_threshold);
        Ok(BoundedClusteringOutcome {
            clusters,
            admission_comparisons,
            score_matrix_rows: scores.len(),
            input_count: selection.notes.len(),
            overflow_count: selection.overflow_count,
        })
    }

    // ── T3: attempt identity and the atomic canonical transaction ───────────

    /// Locate the immutable consolidation creation revision carrying
    /// `attempt_id`, together with the exact `supersedes` endpoint set and
    /// canonical provenance of the note it created.
    pub async fn find_consolidation_attempt(
        &self,
        attempt_id: &str,
    ) -> Result<Option<ConsolidationAttemptWitness>> {
        self.db.ensure_initialized().await?;

        let row: Option<AttemptWitnessRow> = sqlx::query_as(
            r#"SELECT nre.id AS revision_id, n.id AS note_id, n.project_id, n.note_type,
                      nre.session_id, n.content
               FROM note_revision_events nre
               JOIN notes n ON n.id = nre.note_id
               WHERE nre.consolidation_attempt_id = $1"#,
        )
        .bind(attempt_id)
        .fetch_optional(self.db.pool())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };

        // A `supersedes` edge is stored canonical-pair normalized, so the
        // canonical is whichever endpoint is not the source. Reading both
        // columns and subtracting the canonical id recovers the exact endpoint
        // set without depending on column order.
        let mut endpoints: Vec<String> = sqlx::query_scalar(
            r#"SELECT CASE WHEN note_a_id = $1 THEN note_b_id ELSE note_a_id END
               FROM note_associations
               WHERE kind = 'supersedes' AND (note_a_id = $1 OR note_b_id = $1)"#,
        )
        .bind(&row.note_id)
        .fetch_all(self.db.pool())
        .await?;
        endpoints.sort();
        endpoints.dedup();

        let provenance_session_ids: Vec<String> = sqlx::query_scalar(
            "SELECT session_id FROM consolidated_note_provenance WHERE note_id = $1 \
             ORDER BY session_id ASC",
        )
        .bind(&row.note_id)
        .fetch_all(self.db.pool())
        .await?;

        Ok(Some(ConsolidationAttemptWitness {
            canonical_note_id: row.note_id,
            creation_revision_id: row.revision_id,
            project_id: row.project_id,
            note_type: row.note_type,
            session_id: row.session_id,
            canonical_body_digest: note_content_hash(&row.content),
            supersedes_source_note_ids: endpoints,
            canonical_provenance_session_ids: provenance_session_ids,
        }))
    }

    /// Commit — atomically — the canonical note, its unique attempt identity and
    /// immutable consolidation-attributed creation revision, the display tag,
    /// the summary fields, the canonical provenance rows, one `supersedes` edge
    /// per source, and every source's `active → superseded` revisioned
    /// transition.
    ///
    /// Any validation or database error rolls back **all six** effects. Sources
    /// are locked `FOR UPDATE` in ascending id order so concurrent runners
    /// serialize on the same rows; the unique `consolidation_attempt_id` index
    /// is a second defense against duplicate identity.
    pub async fn commit_consolidation_canonical(
        &self,
        request: CommitConsolidationCanonical<'_>,
    ) -> Result<ConsolidationCommitOutcome> {
        request.partition.validate()?;
        self.db.ensure_initialized().await?;

        let mut sorted_sources = request.source_note_ids.to_vec();
        sorted_sources.sort();
        sorted_sources.dedup();
        if sorted_sources.len() != request.source_note_ids.len() {
            return Err(Error::InvalidData(
                "consolidation source note ids must be unique".to_owned(),
            ));
        }
        if sorted_sources.len() < CONSOLIDATION_MIN_CLUSTER_SOURCES
            || sorted_sources.len() > CONSOLIDATION_MAX_CLUSTER_SOURCES
        {
            return Err(Error::InvalidData(format!(
                "a qualifying consolidation cluster has {CONSOLIDATION_MIN_CLUSTER_SOURCES}–{CONSOLIDATION_MAX_CLUSTER_SOURCES} sources, got {}",
                sorted_sources.len()
            )));
        }

        let canonical_body_digest = note_content_hash(request.content);
        let attempt_id = consolidation_attempt_id(
            request.partition,
            &sorted_sources,
            &canonical_body_digest,
        );

        // An ambiguous retry must be decided by exact attempt identity, never by
        // source status.
        if let Some(witness) = self.find_consolidation_attempt(&attempt_id).await? {
            return Ok(self.classify_attempt_witness(
                &witness,
                request.partition,
                &sorted_sources,
                &canonical_body_digest,
                &attempt_id,
            )?);
        }

        let mut tx = self.db.pool().begin().await?;

        let locked: Vec<LockedSourceRow> = sqlx::query_as(
            r#"SELECT n.id, n.project_id, n.note_type, n.storage, n.status, n.content, n.confidence,
                      EXISTS (
                        SELECT 1 FROM consolidated_note_provenance cnp
                        JOIN sessions s ON s.id = cnp.session_id
                        WHERE cnp.note_id = n.id
                          AND cnp.session_id = $2
                          AND s.project_id = n.project_id
                      ) AS in_partition_session,
                      EXISTS (
                        SELECT 1 FROM note_revision_events nre
                        WHERE nre.note_id = n.id
                          AND nre.event_kind = 'created'
                          AND nre.actor_kind = 'system'
                          AND nre.subsystem = 'consolidation'
                      ) AS consolidation_attributed
               FROM notes n
               WHERE n.id = ANY($1::text[])
               ORDER BY n.id ASC
               FOR UPDATE OF n"#,
        )
        .bind(&sorted_sources)
        .bind(&request.partition.session_id)
        .fetch_all(&mut *tx)
        .await?;

        let observed_source_statuses = locked
            .iter()
            .map(|row| (row.id.clone(), row.status.clone()))
            .collect::<Vec<_>>();

        if locked.len() != sorted_sources.len() {
            tx.rollback().await?;
            return Ok(ConsolidationCommitOutcome::Conflict(ConsolidationConflict {
                reason: ConsolidationConflictReason::SourceNotEligible,
                detail: format!(
                    "{} of {} requested sources are missing",
                    sorted_sources.len() - locked.len(),
                    sorted_sources.len()
                ),
                observed_source_statuses: observed_source_statuses.clone(),
            }));
        }

        for row in &locked {
            if row.project_id != request.partition.project_id
                || row.note_type != request.partition.note_type
            {
                tx.rollback().await?;
                return Ok(ConsolidationCommitOutcome::Conflict(ConsolidationConflict {
                    reason: ConsolidationConflictReason::PartitionMismatch,
                    detail: format!(
                        "source {} is {}/{}, not {}/{}",
                        row.id,
                        row.project_id,
                        row.note_type,
                        request.partition.project_id,
                        request.partition.note_type
                    ),
                    observed_source_statuses: observed_source_statuses.clone(),
                }));
            }
            if row.status != "active"
                || row.storage != "db"
                || row.consolidation_attributed
                || !row.in_partition_session
            {
                tx.rollback().await?;
                return Ok(ConsolidationCommitOutcome::Conflict(ConsolidationConflict {
                    reason: ConsolidationConflictReason::SourceNotEligible,
                    detail: format!(
                        "source {} failed revalidation (status={}, storage={}, consolidation_attributed={}, in_partition_session={})",
                        row.id,
                        row.status,
                        row.storage,
                        row.consolidation_attributed,
                        row.in_partition_session
                    ),
                    observed_source_statuses: observed_source_statuses.clone(),
                }));
            }
        }

        let committed = match self
            .write_canonical_transaction(
                &mut tx,
                &request,
                &sorted_sources,
                &locked,
                &attempt_id,
                &canonical_body_digest,
            )
            .await
        {
            Ok(committed) => committed,
            Err(error) => {
                tx.rollback().await?;
                return Err(error);
            }
        };

        tx.commit().await?;
        Ok(ConsolidationCommitOutcome::Committed(committed))
    }

    fn classify_attempt_witness(
        &self,
        witness: &ConsolidationAttemptWitness,
        partition: &ConsolidationPartitionKey,
        sorted_sources: &[String],
        canonical_body_digest: &str,
        attempt_id: &str,
    ) -> Result<ConsolidationCommitOutcome> {
        let partition_matches = witness.project_id == partition.project_id
            && witness.note_type == partition.note_type
            && witness.session_id.as_deref() == Some(partition.session_id.as_str());
        let digest_matches = witness.canonical_body_digest == canonical_body_digest;
        let endpoints_match = witness.supersedes_source_note_ids == sorted_sources;

        if partition_matches && digest_matches && endpoints_match {
            return Ok(ConsolidationCommitOutcome::AlreadyCommitted(
                CommittedConsolidationCanonical {
                    canonical_note_id: witness.canonical_note_id.clone(),
                    consolidation_attempt_id: attempt_id.to_owned(),
                    canonical_body_digest: canonical_body_digest.to_owned(),
                    creation_revision_id: witness.creation_revision_id.clone(),
                    canonical_provenance_session_ids: witness
                        .canonical_provenance_session_ids
                        .clone(),
                    supersedes_source_note_ids: witness.supersedes_source_note_ids.clone(),
                    final_source_statuses: Vec::new(),
                },
            ));
        }

        Ok(ConsolidationCommitOutcome::Conflict(ConsolidationConflict {
            reason: ConsolidationConflictReason::AttemptIdentityMismatch,
            detail: format!(
                "attempt {attempt_id} located canonical {} but partition_matches={partition_matches}, digest_matches={digest_matches}, endpoints_match={endpoints_match}",
                witness.canonical_note_id
            ),
            observed_source_statuses: Vec::new(),
        }))
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_canonical_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        request: &CommitConsolidationCanonical<'_>,
        sorted_sources: &[String],
        locked: &[LockedSourceRow],
        attempt_id: &str,
        canonical_body_digest: &str,
    ) -> Result<CommittedConsolidationCanonical> {
        let partition = request.partition;
        let canonical_id = uuid::Uuid::now_v7().to_string();
        let tags_json =
            serde_json::Value::Array(vec![serde_json::Value::String(
                CONSOLIDATION_CANONICAL_TAG.to_owned(),
            )]);
        let scope_paths_json: serde_json::Value = serde_json::from_str(request.scope_paths)
            .map_err(|e| Error::InvalidData(format!("invalid json for notes.scope_paths: {e}")))?;
        let confidence = request
            .confidence
            .clamp(CONFIDENCE_FLOOR, CONFIDENCE_CEILING);

        // ── 1. canonical note ───────────────────────────────────────────────
        self.fail_at(ConsolidationWriteBoundary::CanonicalNoteInsert)?;
        sqlx::query(
            "INSERT INTO notes (id, project_id, permalink, title, file_path, storage, note_type, \
             folder, status, tags, content, retrieval_anchor, content_hash, scope_paths, confidence) \
             VALUES ($1, $2, $3, $4, '', 'db', $5, $6, 'active', $7, $8, NULL, $9, $10, $11)",
        )
        .bind(&canonical_id)
        .bind(&partition.project_id)
        .bind(permalink_for(&partition.note_type, request.title))
        .bind(request.title)
        .bind(&partition.note_type)
        .bind(folder_for_type(&partition.note_type))
        .bind(tags_json)
        .bind(request.content)
        .bind(canonical_body_digest)
        .bind(scope_paths_json)
        .bind(confidence)
        .execute(&mut **tx)
        .await?;
        index_links_for_note(tx, &canonical_id, &partition.project_id, request.content).await?;
        resolve_links_for_note(
            tx,
            &canonical_id,
            request.title,
            &permalink_for(&partition.note_type, request.title),
            &partition.project_id,
        )
        .await?;

        // ── 2. immutable creation revision carrying the attempt identity ────
        self.fail_at(ConsolidationWriteBoundary::CanonicalCreationRevision)?;
        let creation_revision_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO note_revision_events (id, project_id, note_id, note_seq, event_kind, \
             content_before, content_after, confidence_before, confidence_after, actor_kind, \
             actor_id, subsystem, session_id, task_id, task_run_id, reason, \
             consolidation_attempt_id) \
             VALUES ($1, $2, $3, 1, 'created', NULL, $4, NULL, $5, 'system', NULL, \
             'consolidation', $6, NULL, NULL, $7, $8)",
        )
        .bind(&creation_revision_id)
        .bind(&partition.project_id)
        .bind(&canonical_id)
        .bind(request.content)
        .bind(confidence)
        .bind(&partition.session_id)
        .bind(request.reason.as_str())
        .bind(attempt_id)
        .execute(&mut **tx)
        .await?;

        // ── 3. canonical summary fields ─────────────────────────────────────
        self.fail_at(ConsolidationWriteBoundary::CanonicalSummaryFields)?;
        sqlx::query("UPDATE notes SET abstract = $1, overview = $2 WHERE id = $3")
            .bind(request.abstract_)
            .bind(request.overview)
            .bind(&canonical_id)
            .execute(&mut **tx)
            .await?;

        // ── 4. canonical provenance rows ────────────────────────────────────
        //
        // The union of every source's same-project session membership. This
        // always includes the selected session, which is what makes the new
        // canonical visible to a later sweep for that session.
        self.fail_at(ConsolidationWriteBoundary::CanonicalProvenanceRows)?;
        let mut canonical_provenance_session_ids: Vec<String> = sqlx::query_scalar(
            r#"INSERT INTO consolidated_note_provenance (note_id, session_id, created_at)
               SELECT DISTINCT $1, cnp.session_id,
                      to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               FROM consolidated_note_provenance cnp
               JOIN sessions s ON s.id = cnp.session_id
               WHERE cnp.note_id = ANY($2::text[])
                 AND s.project_id = $3
               ON CONFLICT (note_id, session_id) DO NOTHING
               RETURNING session_id"#,
        )
        .bind(&canonical_id)
        .bind(sorted_sources)
        .bind(&partition.project_id)
        .fetch_all(&mut **tx)
        .await?;
        canonical_provenance_session_ids.sort();
        canonical_provenance_session_ids.dedup();

        // ── 5. one `supersedes` edge per source ─────────────────────────────
        self.fail_at(ConsolidationWriteBoundary::SupersedesEdges)?;
        for source_id in sorted_sources {
            self.note_repo
                .record_supersedes_in_transaction(tx, &canonical_id, source_id, 1.0)
                .await?;
        }

        // ── 6. every source active → superseded, with its revision ──────────
        self.fail_at(ConsolidationWriteBoundary::SourceStatusTransition)?;
        let mut final_source_statuses = Vec::with_capacity(locked.len());
        for row in locked {
            let transitioned = sqlx::query(
                r#"UPDATE notes
                   SET status = 'superseded',
                       lifecycle_changed_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                       updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                   WHERE id = $1 AND project_id = $2 AND status = 'active'"#,
            )
            .bind(&row.id)
            .bind(&partition.project_id)
            .execute(&mut **tx)
            .await?
            .rows_affected();
            if transitioned != 1 {
                return Err(Error::Internal(format!(
                    "source {} was not transitioned active → superseded under lock",
                    row.id
                )));
            }
            let seq: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(note_seq), 0) + 1 FROM note_revision_events \
                 WHERE project_id = $1 AND note_id = $2",
            )
            .bind(&partition.project_id)
            .bind(&row.id)
            .fetch_one(&mut **tx)
            .await?;
            sqlx::query(
                "INSERT INTO note_revision_events (id, project_id, note_id, note_seq, event_kind, \
                 content_before, content_after, confidence_before, confidence_after, actor_kind, \
                 actor_id, subsystem, session_id, task_id, task_run_id, reason) \
                 VALUES ($1, $2, $3, $4, 'updated', $5, $5, $6, $6, 'system', NULL, \
                 'consolidation', $7, NULL, NULL, $8)",
            )
            .bind(uuid::Uuid::now_v7().to_string())
            .bind(&partition.project_id)
            .bind(&row.id)
            .bind(seq)
            .bind(&row.content)
            .bind(row.confidence)
            .bind(&partition.session_id)
            .bind(request.reason.as_str())
            .execute(&mut **tx)
            .await?;
            final_source_statuses.push((row.id.clone(), "superseded".to_owned()));
        }
        final_source_statuses.sort();

        Ok(CommittedConsolidationCanonical {
            canonical_note_id: canonical_id,
            consolidation_attempt_id: attempt_id.to_owned(),
            canonical_body_digest: canonical_body_digest.to_owned(),
            creation_revision_id,
            canonical_provenance_session_ids,
            supersedes_source_note_ids: sorted_sources.to_vec(),
            final_source_statuses,
        })
    }

    fn fail_at(&self, boundary: ConsolidationWriteBoundary) -> Result<()> {
        if self.canonical_write_failure.load(std::sync::atomic::Ordering::SeqCst) == boundary.code()
        {
            return Err(Error::Internal(format!(
                "forced consolidation write failure at boundary {boundary:?}"
            )));
        }
        Ok(())
    }

    /// Test-only deterministic seam: fail the canonical transaction at exactly
    /// one write boundary so the fixtures can prove that none of the six
    /// effects survives a rollback.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_canonical_write_failure_for_test(
        &self,
        boundary: Option<ConsolidationWriteBoundary>,
    ) {
        self.canonical_write_failure.store(
            boundary.map_or(0, ConsolidationWriteBoundary::code),
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    // ── T6: passive partition pressure ──────────────────────────────────────

    /// Report per-`(project_id, note_type)` retrieval pressure from **one**
    /// statement snapshot.
    ///
    /// `injectable_slots` is the caller's configured per-type context-build
    /// budget. When it is zero the reading carries `unbounded_pressure = true`
    /// and no numeric ratio.
    ///
    /// This is read-only and passive: it creates no prompt policy, task,
    /// cooldown, deletion, or automatic actuator, and it is never invoked on the
    /// write path.
    pub async fn partition_pressure_metrics(
        &self,
        injectable_slots_by_note_type: &HashMap<String, i64>,
    ) -> Result<Vec<PartitionPressureMetric>> {
        self.db.ensure_initialized().await?;

        let rows: Vec<PartitionPressureRow> = sqlx::query_as(
            r#"SELECT n.project_id, n.note_type, COUNT(*) AS eligible_notes
               FROM notes n
               WHERE n.storage = 'db'
                 AND n.status = 'active'
                 AND n.note_type = ANY($1::text[])
                 AND NOT EXISTS (
                       SELECT 1 FROM note_revision_events nre
                       WHERE nre.note_id = n.id
                         AND nre.event_kind = 'created'
                         AND nre.actor_kind = 'system'
                         AND nre.subsystem = 'consolidation'
                     )
               GROUP BY n.project_id, n.note_type
               ORDER BY n.project_id ASC, n.note_type ASC"#,
        )
        .bind(eligible_note_type_array())
        .fetch_all(self.db.pool())
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let injectable_slots = injectable_slots_by_note_type
                    .get(&row.note_type)
                    .copied()
                    .unwrap_or(0)
                    .max(0);
                let (oversubscription_ratio, unbounded_pressure) = if injectable_slots > 0 {
                    (
                        Some(row.eligible_notes as f64 / injectable_slots as f64),
                        false,
                    )
                } else {
                    (None, true)
                };
                PartitionPressureMetric {
                    project_id: row.project_id,
                    note_type: row.note_type,
                    eligible_notes: row.eligible_notes,
                    injectable_slots,
                    oversubscription_ratio,
                    unbounded_pressure,
                }
            })
            .collect())
    }
}

// ── Free functions ───────────────────────────────────────────────────────────

fn eligible_note_type_array() -> Vec<String> {
    CONSOLIDATION_ELIGIBLE_NOTE_TYPES
        .iter()
        .map(|value| (*value).to_owned())
        .collect()
}

/// Query text for one partition seed. Identical to the text the previous
/// per-seed `dedup_candidates_for_group` call built, so the `ts_rank` semantics
/// are preserved exactly.
pub(super) fn build_partition_query_text(note: &ConsolidationNote) -> String {
    let mut query_text = String::new();
    if let Some(abstract_) = note.abstract_.as_deref() {
        query_text.push_str(abstract_);
        query_text.push(' ');
    }
    if let Some(overview) = note.overview.as_deref() {
        query_text.push_str(overview);
        query_text.push(' ');
    }
    query_text.push_str(&note.title);
    query_text.push(' ');
    query_text.push_str(&note.content);
    query_text
}

/// `SHA-256(version || project_id || session_id || note_type || sorted_source_ids
/// || canonical_body_digest)`.
///
/// Every component is length-prefixed so no two different tuples can serialize
/// to the same byte string.
pub fn consolidation_attempt_id(
    partition: &ConsolidationPartitionKey,
    sorted_source_ids: &[String],
    canonical_body_digest: &str,
) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let mut absorb = |value: &str| {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    };
    absorb(CONSOLIDATION_ATTEMPT_VERSION);
    absorb(&partition.project_id);
    absorb(&partition.session_id);
    absorb(&partition.note_type);
    absorb(&sorted_source_ids.len().to_string());
    for source_id in sorted_source_ids {
        absorb(source_id);
    }
    absorb(canonical_body_digest);
    format!("{:x}", hasher.finalize())
}

/// Deterministic greedy **complete-link** grouping over a materialized directed
/// score matrix.
///
/// Seeds are iterated by `(permalink, id)`. A candidate joins only when it meets
/// the threshold against **every** current member, tested in the seed-to-
/// candidate direction the old per-seed iteration would have queried. Growth
/// stops at [`CONSOLIDATION_MAX_CLUSTER_SOURCES`]; a group smaller than
/// [`CONSOLIDATION_MIN_CLUSTER_SOURCES`] is discarded and only its seed is
/// consumed, so its other members remain available to later seeds.
///
/// Because every admission test is a `(member, candidate)` pair with
/// `member_index < candidate_index` and results are memoized, each unordered
/// pair is evaluated at most once. The returned comparison count is therefore
/// bounded by `C(n, 2)`, i.e. at most
/// [`CONSOLIDATION_MAX_ADMISSION_COMPARISONS`] for a full 200-input partition.
///
/// A bridge chain cannot merge through one shared neighbor (every admitted
/// source must match every existing member) and a dense component is split into
/// deterministic groups of at most eight.
///
/// Returns `(qualifying_clusters, admission_comparisons)`.
pub fn build_bounded_clusters(
    notes: &[ConsolidationNote],
    scores: &[DirectedScoreRow],
    threshold: f64,
) -> (Vec<BoundedCluster>, usize) {
    if notes.len() < CONSOLIDATION_MIN_CLUSTER_SOURCES {
        return (Vec::new(), 0);
    }

    let mut ordered = notes.to_vec();
    ordered.sort_by(|left, right| {
        left.permalink
            .cmp(&right.permalink)
            .then_with(|| left.id.cmp(&right.id))
    });
    let index_of: HashMap<&str, usize> = ordered
        .iter()
        .enumerate()
        .map(|(index, note)| (note.id.as_str(), index))
        .collect();

    let mut directed: HashMap<(usize, usize), f64> = HashMap::new();
    for row in scores {
        let (Some(&seed), Some(&candidate)) = (
            index_of.get(row.seed_id.as_str()),
            index_of.get(row.candidate_id.as_str()),
        ) else {
            continue;
        };
        directed
            .entry((seed, candidate))
            .and_modify(|existing| *existing = existing.max(row.score))
            .or_insert(row.score);
    }

    let mut consumed = vec![false; ordered.len()];
    // Memoized unordered admission results. `member < candidate` always holds,
    // so the tuple is already the canonical unordered key.
    let mut evaluated: HashMap<(usize, usize), bool> = HashMap::new();
    let mut admission_comparisons = 0usize;
    let mut clusters = Vec::new();

    for seed in 0..ordered.len() {
        if consumed[seed] {
            continue;
        }
        // The seed is spent by this attempt whether or not the group qualifies,
        // which is what makes the sweep terminate and stay deterministic.
        consumed[seed] = true;
        let mut members = vec![seed];

        for candidate in (seed + 1)..ordered.len() {
            if members.len() >= CONSOLIDATION_MAX_CLUSTER_SOURCES {
                break;
            }
            if consumed[candidate] {
                continue;
            }
            let mut admitted = true;
            for &member in &members {
                let key = (member, candidate);
                let outcome = match evaluated.get(&key) {
                    Some(cached) => *cached,
                    None => {
                        admission_comparisons += 1;
                        let score = directed.get(&key).copied().unwrap_or(0.0);
                        let outcome = score >= threshold;
                        evaluated.insert(key, outcome);
                        outcome
                    }
                };
                if !outcome {
                    admitted = false;
                    break;
                }
            }
            if admitted {
                members.push(candidate);
            }
        }

        if members.len() < CONSOLIDATION_MIN_CLUSTER_SOURCES {
            continue;
        }
        for &member in &members {
            consumed[member] = true;
        }
        let ordered_notes = members
            .iter()
            .map(|&index| ordered[index].clone())
            .collect::<Vec<_>>();
        let mut source_note_ids = ordered_notes
            .iter()
            .map(|note| note.id.clone())
            .collect::<Vec<_>>();
        source_note_ids.sort();
        clusters.push(BoundedCluster {
            source_note_ids,
            ordered_notes,
        });
    }

    // Deterministic run order: first member's `(permalink, id)`, then the full
    // sorted source-ID tuple.
    clusters.sort_by(|left, right| {
        let left_head = &left.ordered_notes[0];
        let right_head = &right.ordered_notes[0];
        left_head
            .permalink
            .cmp(&right_head.permalink)
            .then_with(|| left_head.id.cmp(&right_head.id))
            .then_with(|| left.source_note_ids.cmp(&right.source_note_ids))
    });

    (clusters, admission_comparisons)
}

/// Connected components over a materialized directed score matrix, preserving
/// the exact grouping semantics the per-seed `dedup_candidates_for_group` loop
/// produced for the corpus audit.
///
/// This is deliberately *not* the complete-link admission used by the canonical
/// transaction. The audit reports likely-duplicate neighborhoods, not merge
/// sets, so changing its grouping rule would change its semantic report. Only
/// the round-trip fan-out changes: the same edges are now derived from one
/// set-based query per type instead of one query per seed.
///
/// Each returned component holds at least two note IDs ordered by
/// `(permalink, id)`; components are ordered by their ID tuple.
pub fn connected_components_from_score_matrix(
    notes: &[ConsolidationNote],
    scores: &[DirectedScoreRow],
) -> Vec<Vec<String>> {
    use std::collections::{BTreeMap, VecDeque};

    let by_id: HashMap<&str, &ConsolidationNote> = notes
        .iter()
        .map(|note| (note.id.as_str(), note))
        .collect();

    let mut adjacency: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in scores {
        if row.seed_id == row.candidate_id
            || !by_id.contains_key(row.seed_id.as_str())
            || !by_id.contains_key(row.candidate_id.as_str())
        {
            continue;
        }
        adjacency
            .entry(row.seed_id.clone())
            .or_default()
            .insert(row.candidate_id.clone());
        adjacency
            .entry(row.candidate_id.clone())
            .or_default()
            .insert(row.seed_id.clone());
    }

    let mut visited: HashSet<String> = HashSet::new();
    let mut components: Vec<Vec<String>> = Vec::new();
    for start in adjacency.keys() {
        if !visited.insert(start.clone()) {
            continue;
        }
        let mut queue = VecDeque::from([start.clone()]);
        let mut component_ids = BTreeSet::from([start.clone()]);
        while let Some(node) = queue.pop_front() {
            if let Some(neighbors) = adjacency.get(&node) {
                for neighbor in neighbors {
                    if visited.insert(neighbor.clone()) {
                        queue.push_back(neighbor.clone());
                    }
                    component_ids.insert(neighbor.clone());
                }
            }
        }
        if component_ids.len() < 2 {
            continue;
        }
        let mut ordered = component_ids
            .iter()
            .filter_map(|id| by_id.get(id.as_str()).copied())
            .collect::<Vec<_>>();
        ordered.sort_by(|left, right| {
            left.permalink
                .cmp(&right.permalink)
                .then_with(|| left.id.cmp(&right.id))
        });
        components.push(ordered.into_iter().map(|note| note.id.clone()).collect());
    }
    components.sort();
    components
}

/// Distinct source ids across a set of clusters, used by the run report to
/// prove that deferred clusters are disjoint from the committed one.
pub fn cluster_source_id_set(clusters: &[BoundedCluster]) -> BTreeSet<String> {
    clusters
        .iter()
        .flat_map(|cluster| cluster.source_note_ids.iter().cloned())
        .collect()
}

/// Sanity helper for callers that need to know whether two clusters overlap.
pub fn clusters_are_disjoint(clusters: &[BoundedCluster]) -> bool {
    let mut seen = HashSet::new();
    clusters
        .iter()
        .flat_map(|cluster| cluster.source_note_ids.iter())
        .all(|id| seen.insert(id.clone()))
}
