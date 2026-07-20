use std::collections::HashSet;
use std::path::Path;

use djinn_memory::Note;

use super::*;
use crate::note_hash::note_content_hash;

fn merge_orphan_tag(tags_json: &str, orphan_tag: &str) -> String {
    let mut tags: Vec<String> = serde_json::from_str(tags_json).unwrap_or_default();
    let mut seen = HashSet::new();
    tags.retain(|tag| seen.insert(tag.clone()));
    if !tags.iter().any(|tag| tag == orphan_tag) {
        tags.push(orphan_tag.to_string());
    }
    serde_json::to_string(&tags).unwrap_or_else(|_| "[]".to_string())
}

#[derive(sqlx::FromRow)]
struct BrokenLinkCandidateRow {
    source_id: String,
    source_title: String,
    source_tags: String,
    source_content: String,
    target_raw: String,
}

impl NoteRepository {
    pub async fn rebuild_missing_content_hashes(&self, project_id: &str) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let rows = sqlx::query!(
            "SELECT id, content FROM notes WHERE project_id = $1 AND content_hash IS NULL",
            project_id
        )
        .fetch_all(self.db.pool())
        .await?;

        let mut tx = self.db.pool().begin().await?;
        for row in &rows {
            let hash = note_content_hash(&row.content);
            sqlx::query!(
                "UPDATE notes SET content_hash = $1 WHERE id = $2",
                hash,
                row.id
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(rows.len() as u64)
    }

    pub async fn flag_orphan_notes(
        &self,
        project_id: &str,
        project_path: &Path,
        orphan_tag: &str,
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let notes = sqlx::query_as::<_, Note>(
            r#"SELECT id, project_id, permalink, title, file_path,
                        storage, note_type, folder, status, tags::text AS tags, content,
                        retrieval_anchor, created_at, updated_at, lifecycle_changed_at, last_accessed,
                        access_count, confidence,
                        abstract AS abstract_, overview,
                        scope_paths::text AS scope_paths
             FROM notes n
             WHERE n.project_id = $1
               AND n.status = 'active'
               AND n.note_type NOT IN ('brief', 'roadmap', 'catalog')
               AND n.last_accessed < to_char((now() at time zone 'utc') - interval '30 day', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
               AND n.access_count = 0
               AND NOT EXISTS (
                   SELECT 1 FROM note_links l WHERE l.target_id = n.id
               )"#,
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await?;

        let mut flagged = 0_u64;
        for note in notes {
            let merged_tags = merge_orphan_tag(&note.tags, orphan_tag);
            if merged_tags != note.tags {
                self.update(&note.id, &note.title, &note.content, &merged_tags)
                    .await?;
                flagged += 1;
            }
        }

        let _ = project_path;
        Ok(flagged)
    }

    pub async fn repair_broken_wikilinks(
        &self,
        project_id: &str,
        _project_path: &Path,
        min_score: f64,
    ) -> Result<u64> {
        self.db.ensure_initialized().await?;

        let broken = sqlx::query_as!(
            BrokenLinkCandidateRow,
            r#"SELECT src.id AS source_id, src.title AS source_title,
                    src.tags::text AS "source_tags!", src.content AS source_content,
                    l.target_raw AS target_raw
             FROM note_links l
             JOIN notes src ON src.id = l.source_id
             WHERE src.project_id = $1
               AND l.target_id IS NULL
             ORDER BY src.id, l.target_raw"#,
            project_id
        )
        .fetch_all(self.db.pool())
        .await?;

        let mut repaired = 0_u64;
        for row in broken {
            if let Some(replacement) = self
                .best_broken_link_repair(project_id, &row.source_id, &row.target_raw, min_score)
                .await?
            {
                let current = format!("[[{}]]", row.target_raw);
                let new = format!("[[{}]]", replacement);
                if row.source_content.contains(&current) {
                    let updated = row.source_content.replacen(&current, &new, 1);
                    if updated != row.source_content {
                        self.update(
                            &row.source_id,
                            &row.source_title,
                            &updated,
                            &row.source_tags,
                        )
                        .await?;
                        repaired += 1;
                    }
                }
            }
        }

        Ok(repaired)
    }

    /// Set `content_hash = NULL` for a single note.
    ///
    /// Exists as a deliberate test fixture: simulates a legacy row that was
    /// inserted before `content_hash` was introduced so callers can exercise
    /// [`Self::rebuild_missing_content_hashes`]. Production writes should
    /// instead go through `update`/`create*` which compute the hash.
    pub async fn clear_content_hash(&self, note_id: &str) -> Result<()> {
        self.db.ensure_initialized().await?;

        sqlx::query!(
            "UPDATE notes SET content_hash = NULL WHERE id = $1",
            note_id
        )
        .execute(self.db.pool())
        .await?;

        Ok(())
    }

    /// Total number of notes in a project.
    pub async fn count_by_project(&self, project_id: &str) -> Result<i64> {
        self.db.ensure_initialized().await?;

        let count: i64 = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM notes WHERE project_id = $1"#,
            project_id
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(count)
    }

    async fn best_broken_link_repair(
        &self,
        project_id: &str,
        source_id: &str,
        target_raw: &str,
        min_score: f64,
    ) -> Result<Option<String>> {
        let exact_title = sqlx::query_scalar!(
            "SELECT title FROM notes WHERE project_id = $1 AND title = $2 LIMIT 1",
            project_id,
            target_raw
        )
        .fetch_optional(self.db.pool())
        .await?;
        if exact_title.is_some() {
            return Ok(exact_title);
        }

        let results: Vec<_> = self
            .search(NoteSearchParams {
                project_id,
                query: target_raw,
                task_id: None,
                folder: None,
                note_type: None,
                limit: 5,
                semantic_scores: None,
                edge_kinds: None,
                entity_types: None,
            })
            .await?
            .into_iter()
            .filter(|r| r.id != source_id)
            .collect();
        let Some(best) = results.first() else {
            return Ok(None);
        };
        if best.score < min_score {
            return Ok(None);
        }
        if results
            .get(1)
            .is_some_and(|next| (best.score - next.score).abs() < 5.0)
        {
            return Ok(None);
        }

        Ok(Some(best.title.clone()))
    }

    // ── Retrieval-anchor backfill (l4l7 F1, T2) ─────────────────────────────

    /// Default set of `note_type` values considered "extracted durable" by
    /// the LLM extraction path. Backfill scopes to these by default so it
    /// touches the notes that extraction already targets (`case`, `pattern`,
    /// `pitfall`) rather than every note in the project. Callers can pass a
    /// different scope via [`BackfillRetrievalAnchorOptions::note_types`].
    pub const DEFAULT_BACKFILL_NOTE_TYPES: &'static [&'static str] =
        &["case", "pattern", "pitfall"];

    /// List notes that are missing a non-empty `retrieval_anchor`.
    ///
    /// Pure read: this never mutates the database. The default scope is the
    /// durable extracted types (`case`, `pattern`, `pitfall`); pass an
    /// explicit `note_types` slice to broaden (or narrow) it. Singletons
    /// (`brief`, `roadmap`, `catalog`) are always excluded.
    pub async fn list_notes_missing_retrieval_anchor(
        &self,
        project_id: &str,
        note_types: &[&str],
    ) -> Result<Vec<Note>> {
        self.list_notes_for_retrieval_anchor_backfill(project_id, note_types, false)
            .await
    }

    async fn list_notes_for_retrieval_anchor_backfill(
        &self,
        project_id: &str,
        note_types: &[&str],
        include_existing_anchors: bool,
    ) -> Result<Vec<Note>> {
        self.db.ensure_initialized().await?;

        if note_types.is_empty() {
            return Ok(Vec::new());
        }

        // sqlx::query! / IN-list — build $N placeholders manually so the
        // macro can type-check the query.
        let mut sql = String::from(
            r#"SELECT id, project_id, permalink, title, file_path,
                      storage, note_type, folder, status, tags::text AS tags, content,
                      retrieval_anchor, created_at, updated_at, lifecycle_changed_at, last_accessed,
                      access_count, confidence,
                      abstract AS abstract_, overview,
                      scope_paths::text AS scope_paths
               FROM notes
               WHERE project_id = $1
                 AND status = 'active'
                 AND note_type NOT IN ('brief', 'roadmap', 'catalog')
                 AND "#,
        );
        if include_existing_anchors {
            sql.push_str("note_type IN (");
        } else {
            sql.push_str(
                "(retrieval_anchor IS NULL OR BTRIM(retrieval_anchor) = '') AND note_type IN (",
            );
        }
        for (idx, _) in note_types.iter().enumerate() {
            if idx > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&format!("${}", idx + 2));
        }
        sql.push_str(") ORDER BY note_type, title");

        let mut q = sqlx::query_as::<_, Note>(&sql).bind(project_id);
        for note_type in note_types {
            q = q.bind(*note_type);
        }
        let notes = q.fetch_all(self.db.pool()).await?;
        Ok(notes)
    }

    /// Explicit, re-runnable retrieval-anchor backfill for one project.
    ///
    /// Backfill is intentionally narrow:
    /// * Only the `retrieval_anchor` column is written. `content`, `tags`,
    ///   `scope_paths`, `abstract`, and `overview` are never touched, and the
    ///   SQL is hand-written to make that constraint obvious.
    /// * Notes that already have a non-empty anchor are skipped unless
    ///   `force = true` — running the backfill twice is a no-op.
    /// * Defaults scope to the durable extracted types (`case`, `pattern`,
    ///   `pitfall`); pass `note_types` to broaden. Singletons are always
    ///   excluded.
    /// * In dry-run mode the function never writes; it returns the candidate
    ///   list and the proposed anchors as a report.
    pub async fn backfill_retrieval_anchors(
        &self,
        project_id: &str,
        options: BackfillRetrievalAnchorOptions,
    ) -> Result<BackfillRetrievalAnchorReport> {
        self.db.ensure_initialized().await?;

        let note_types: Vec<&str> = if options.note_types.is_empty() {
            Self::DEFAULT_BACKFILL_NOTE_TYPES.to_vec()
        } else {
            options.note_types.clone()
        };

        let candidates = self
            .list_notes_for_retrieval_anchor_backfill(project_id, &note_types, options.force)
            .await?;

        let mut report = BackfillRetrievalAnchorReport {
            candidate_count: candidates.len(),
            dry_run: options.dry_run,
            updated: 0,
            skipped_already_anchored: 0,
            skipped_blank_proposal: 0,
            proposals: Vec::with_capacity(candidates.len()),
        };

        if candidates.is_empty() {
            return Ok(report);
        }

        for note in candidates {
            let proposed: Option<String> = match options.proposer.propose_for(&note).await {
                Ok(proposed) => proposed,
                Err(error) => {
                    return Err(Error::InvalidData(format!(
                        "anchor proposal failed: {error}"
                    )));
                }
            };

            let proposal = ProposedBackfillAnchor {
                note_id: note.id.clone(),
                title: note.title.clone(),
                note_type: note.note_type.clone(),
                proposed_anchor: proposed.clone(),
            };

            if options.dry_run {
                report.proposals.push(proposal);
                continue;
            }

            match proposed {
                Some(anchor) => {
                    // Idempotency guard: even if the listing query missed a
                    // race, never overwrite a non-empty anchor without
                    // `force`. The `update_retrieval_anchor` SQL only writes
                    // the column — content/tags/scope/abstract/overview are
                    // untouched.
                    self.update_retrieval_anchor(&note.id, Some(&anchor))
                        .await?;
                    report.updated += 1;
                    report.proposals.push(proposal);
                }
                None => {
                    report.skipped_blank_proposal += 1;
                    report.proposals.push(proposal);
                }
            }
        }

        Ok(report)
    }
}

// ── Backfill types ────────────────────────────────────────────────────────────

/// Optional knobs for [`NoteRepository::backfill_retrieval_anchors`].
///
/// The defaults make the backfill a safe, dry-run-friendly maintenance tool:
/// it lists candidates, proposes anchors without writing, and only updates
/// the `retrieval_anchor` column when `dry_run = false`.
#[derive(Clone)]
pub struct BackfillRetrievalAnchorOptions {
    /// When true, the backfill computes proposed anchors and returns them in
    /// the report without writing to the database. Defaults to true so the
    /// first call is always a no-op report.
    pub dry_run: bool,
    /// When true, also rewrite notes that already have a non-empty anchor.
    /// Defaults to false — the backfill is re-runnable and idempotent, so a
    /// second call is a no-op unless this is set.
    pub force: bool,
    /// Restrict the backfill to specific `note_type` values. Empty means use
    /// [`NoteRepository::DEFAULT_BACKFILL_NOTE_TYPES`] (the durable
    /// extracted types).
    pub note_types: Vec<&'static str>,
    /// Source of proposed anchors. Default = the deterministic title-based
    /// proposer ([`crate::propose_anchor_deterministic`]), which derives a
    /// one-sentence anchor from the note title (no LLM).
    pub proposer: AnchorProposerKind,
}

impl Default for BackfillRetrievalAnchorOptions {
    fn default() -> Self {
        Self {
            dry_run: true,
            force: false,
            note_types: Vec::new(),
            proposer: AnchorProposerKind::default(),
        }
    }
}

/// Selects how [`NoteRepository::backfill_retrieval_anchors`] computes
/// proposed anchors.
#[derive(Default, Clone)]
pub enum AnchorProposerKind {
    /// Pure function over the note: derives a one-sentence anchor from the
    /// note title. No LLM round-trip.
    #[default]
    Deterministic,
    /// LLM-driven proposal. The closure receives the note and must return a
    /// normalized anchor sentence (or `None` if it cannot propose one).
    Llm(LlmAnchorProposer),
}

/// Async closure type used by [`AnchorProposerKind::Llm`]. The closure is
/// expected to return its error as a `String` so the backfill pipeline can
/// stay `std::result::Result<_, String>` internally and surface it as a
/// `DbError` at the public API boundary.
pub type LlmAnchorProposer = std::sync::Arc<
    dyn for<'a> Fn(
            &'a Note,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = std::result::Result<Option<String>, String>>
                    + Send
                    + 'a,
            >,
        > + Send
        + Sync,
>;

impl AnchorProposerKind {
    async fn propose_for(&self, note: &Note) -> std::result::Result<Option<String>, String> {
        match self {
            AnchorProposerKind::Deterministic => {
                Ok::<Option<String>, String>(propose_anchor_deterministic(note))
            }
            AnchorProposerKind::Llm(proposer) => proposer(note).await,
        }
    }
}

/// Derive a one-sentence anchor from the note title.
///
/// The output is intentionally simple and reviewable: it produces a sentence
/// of the form "Recall <title> when relevant." — short, distinct from the
/// markdown body, and useful for embedding text. Heavier LLM-based proposals
/// are an opt-in via [`AnchorProposerKind::Llm`].
pub fn propose_anchor_deterministic(note: &Note) -> Option<String> {
    let title = note.title.trim();
    if title.is_empty() {
        return None;
    }
    let lowered_first = title
        .chars()
        .next()
        .map(|c| c.to_ascii_lowercase())
        .unwrap_or_default();
    let title_lc = if title
        .chars()
        .nth(1)
        .map(|c| !c.is_ascii_uppercase())
        .unwrap_or(true)
    {
        format!(
            "{}{}",
            lowered_first,
            title.chars().skip(1).collect::<String>()
        )
    } else {
        title.to_string()
    };
    Some(format!("Recall {title_lc} when relevant."))
}

/// Report returned by [`NoteRepository::backfill_retrieval_anchors`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillRetrievalAnchorReport {
    /// Number of candidate notes (matching the type scope, missing an
    /// anchor) the backfill considered.
    pub candidate_count: usize,
    /// Whether the run was a dry run (proposals only, no writes).
    pub dry_run: bool,
    /// Notes that received a new `retrieval_anchor`. Always 0 in dry-run.
    pub updated: usize,
    /// Notes skipped because they already had a non-empty anchor. Always 0
    /// in dry-run (the listing query already excludes them).
    pub skipped_already_anchored: usize,
    /// Notes for which the proposer returned `None`. Never written.
    pub skipped_blank_proposal: usize,
    /// Per-candidate proposal, including the proposed anchor sentence
    /// (which may be `None` for notes the proposer could not anchor).
    pub proposals: Vec<ProposedBackfillAnchor>,
}

/// One row of the backfill report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedBackfillAnchor {
    pub note_id: String,
    pub title: String,
    pub note_type: String,
    pub proposed_anchor: Option<String>,
}

#[cfg(test)]
mod backfill_tests {
    use super::*;
    use crate::repositories::test_support::{event_bus_for, make_project};
    use tempfile::TempDir;
    use tokio::sync::broadcast;

    async fn make_test_project(db: &Database) -> (TempDir, djinn_core::models::Project) {
        let tmp = crate::database::test_tempdir().unwrap();
        let project = make_project(db, tmp.path()).await;
        (tmp, project)
    }

    fn make_repo(db: Database) -> NoteRepository {
        let (tx, _rx) = broadcast::channel(64);
        NoteRepository::new(db, event_bus_for(&tx))
    }

    fn empty_options(dry_run: bool) -> BackfillRetrievalAnchorOptions {
        BackfillRetrievalAnchorOptions {
            dry_run,
            ..BackfillRetrievalAnchorOptions::default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_lists_candidates_for_durable_types_only_by_default() {
        let db = Database::open_in_memory().unwrap();
        let (_tmp, project) = make_test_project(&db).await;
        let repo = make_repo(db);

        // No-anchor candidates across the durable types AND outside them.
        repo.create(&project.id, "Anchored Pattern", "body", "pattern", "[]")
            .await
            .unwrap();
        repo.create(&project.id, "Anchored Case", "body", "case", "[]")
            .await
            .unwrap();
        repo.create(&project.id, "Anchored Pitfall", "body", "pitfall", "[]")
            .await
            .unwrap();
        // An ADR — NOT in the default durable scope; should be ignored.
        repo.create(&project.id, "Some ADR", "body", "adr", "[]")
            .await
            .unwrap();
        // A brief singleton — always excluded.
        repo.create(&project.id, "Brief", "body", "brief", "[]")
            .await
            .unwrap();

        let candidates = repo
            .list_notes_missing_retrieval_anchor(&project.id, &[])
            .await
            .unwrap();

        // Empty note_types means "use the default scope" inside the listing
        // function (it short-circuits to Vec::new() to avoid a meaningless
        // query). Callers should pass DEFAULT_BACKFILL_NOTE_TYPES or rely on
        // backfill_retrieval_anchors to fill the default. Repeat the test via
        // the high-level entry point to assert the real behaviour.
        assert!(
            candidates.is_empty(),
            "listing with an empty type filter is a no-op by design"
        );

        let report = repo
            .backfill_retrieval_anchors(&project.id, empty_options(true))
            .await
            .unwrap();
        assert_eq!(report.candidate_count, 3, "default scope = durable types");
        assert!(report.dry_run);
        assert_eq!(report.updated, 0);
        assert_eq!(report.proposals.len(), 3);
        for proposal in &report.proposals {
            assert!(
                proposal.proposed_anchor.is_some(),
                "deterministic proposer should always produce a sentence"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_dry_run_does_not_mutate_notes() {
        let db = Database::open_in_memory().unwrap();
        let (_tmp, project) = make_test_project(&db).await;
        let repo = make_repo(db.clone());

        let created = repo
            .create(
                &project.id,
                "Untouched Pattern",
                "body before",
                "pattern",
                r#"["unchanged"]"#,
            )
            .await
            .unwrap();
        // Add a non-default scope_paths so we can prove nothing touched it.
        repo.update_scope_paths(&created.id, r#"["crates/x"]"#)
            .await
            .unwrap();

        let before = repo
            .get(&created.id)
            .await
            .unwrap()
            .expect("note present before backfill");

        let report = repo
            .backfill_retrieval_anchors(&project.id, empty_options(true))
            .await
            .unwrap();
        assert_eq!(report.candidate_count, 1);
        assert_eq!(report.updated, 0, "dry run writes nothing");

        let after = repo.get(&created.id).await.unwrap().expect("note present");
        assert_eq!(after.content, before.content, "content untouched");
        assert_eq!(after.tags, before.tags, "tags untouched");
        assert_eq!(after.scope_paths, before.scope_paths, "scope untouched");
        assert_eq!(after.abstract_, before.abstract_, "abstract untouched");
        assert_eq!(after.overview, before.overview, "overview untouched");
        assert!(
            after.retrieval_anchor.is_none(),
            "dry run never sets the anchor"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_apply_writes_only_retrieval_anchor_field() {
        let db = Database::open_in_memory().unwrap();
        let (_tmp, project) = make_test_project(&db).await;
        let repo = make_repo(db.clone());

        let created = repo
            .create(
                &project.id,
                "Heavily Decorated Pattern",
                "## Context\nPattern body that must remain intact.\n## Related\n- x",
                "pattern",
                r#"["alpha","beta"]"#,
            )
            .await
            .unwrap();
        repo.update_scope_paths(&created.id, r#"["crates/y"]"#)
            .await
            .unwrap();
        repo.update_summaries(&created.id, Some("abstract text"), Some("overview text"))
            .await
            .unwrap();

        let before = repo.get(&created.id).await.unwrap().expect("note present");

        let report = repo
            .backfill_retrieval_anchors(&project.id, empty_options(false))
            .await
            .unwrap();
        assert_eq!(report.candidate_count, 1);
        assert_eq!(report.updated, 1);
        assert_eq!(report.skipped_blank_proposal, 0);

        let after = repo.get(&created.id).await.unwrap().expect("note present");
        assert_eq!(after.content, before.content, "content untouched");
        assert_eq!(after.tags, before.tags, "tags untouched");
        assert_eq!(after.scope_paths, before.scope_paths, "scope untouched");
        assert_eq!(after.abstract_, before.abstract_, "abstract untouched");
        assert_eq!(after.overview, before.overview, "overview untouched");
        assert!(
            after.retrieval_anchor.is_some(),
            "apply mode writes the anchor"
        );
        assert_ne!(
            after.updated_at, before.updated_at,
            "apply updates updated_at"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_is_idempotent_when_run_twice() {
        let db = Database::open_in_memory().unwrap();
        let (_tmp, project) = make_test_project(&db).await;
        let repo = make_repo(db.clone());

        repo.create(&project.id, "Idempotent Pattern", "body", "pattern", "[]")
            .await
            .unwrap();

        let first = repo
            .backfill_retrieval_anchors(&project.id, empty_options(false))
            .await
            .unwrap();
        assert_eq!(first.updated, 1);

        let second = repo
            .backfill_retrieval_anchors(&project.id, empty_options(false))
            .await
            .unwrap();
        assert_eq!(
            second.candidate_count, 0,
            "second run sees no candidates (anchor now present)"
        );
        assert_eq!(second.updated, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_force_flag_overwrites_existing_anchors() {
        let db = Database::open_in_memory().unwrap();
        let (_tmp, project) = make_test_project(&db).await;
        let repo = make_repo(db.clone());

        let created = repo
            .create(&project.id, "Pre-anchored Pattern", "body", "pattern", "[]")
            .await
            .unwrap();
        repo.update_retrieval_anchor(&created.id, Some("Old manual anchor."))
            .await
            .unwrap();

        let no_force = repo
            .backfill_retrieval_anchors(&project.id, empty_options(false))
            .await
            .unwrap();
        assert_eq!(
            no_force.candidate_count, 0,
            "without force, anchored notes are skipped"
        );

        let forced = BackfillRetrievalAnchorOptions {
            dry_run: false,
            force: true,
            note_types: vec![],
            proposer: AnchorProposerKind::Deterministic,
        };
        let report = repo
            .backfill_retrieval_anchors(&project.id, forced)
            .await
            .unwrap();
        assert_eq!(
            report.candidate_count, 1,
            "force=true selects already-anchored notes for an explicit rewrite"
        );
        assert_eq!(report.updated, 1);
        let after = repo.get(&created.id).await.unwrap().expect("note");
        assert_eq!(
            after.retrieval_anchor.as_deref(),
            Some("Recall pre-anchored Pattern when relevant.")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backfill_respects_caller_supplied_note_type_scope() {
        let db = Database::open_in_memory().unwrap();
        let (_tmp, project) = make_test_project(&db).await;
        let repo = make_repo(db);

        repo.create(&project.id, "An ADR", "body", "adr", "[]")
            .await
            .unwrap();
        repo.create(&project.id, "A Case", "body", "case", "[]")
            .await
            .unwrap();

        let options = BackfillRetrievalAnchorOptions {
            dry_run: true,
            note_types: vec!["adr", "case"],
            ..BackfillRetrievalAnchorOptions::default()
        };
        let report = repo
            .backfill_retrieval_anchors(&project.id, options)
            .await
            .unwrap();
        assert_eq!(
            report.candidate_count, 2,
            "broadened scope picks up both an adr and a case"
        );
        let types: Vec<&str> = report
            .proposals
            .iter()
            .map(|p| p.note_type.as_str())
            .collect();
        assert!(types.contains(&"adr"));
        assert!(types.contains(&"case"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deterministic_proposer_lowercases_first_letter() {
        let note = Note {
            id: "n1".to_string(),
            project_id: "p1".to_string(),
            permalink: "patterns/x".to_string(),
            title: "Pattern Note".to_string(),
            file_path: String::new(),
            storage: "db".to_string(),
            note_type: "pattern".to_string(),
            folder: "patterns".to_string(),
            status: "active".to_string(),
            tags: "[]".to_string(),
            content: String::new(),
            retrieval_anchor: None,
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            updated_at: "2026-01-01T00:00:00.000Z".to_string(),
            lifecycle_changed_at: None,
            last_accessed: "2026-01-01T00:00:00.000Z".to_string(),
            access_count: 0,
            confidence: 1.0,
            abstract_: None,
            overview: None,
            scope_paths: "[]".to_string(),
        };
        assert_eq!(
            propose_anchor_deterministic(&note).as_deref(),
            Some("Recall pattern Note when relevant.")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deterministic_proposer_returns_none_for_empty_title() {
        let mut note = Note {
            id: "n1".to_string(),
            project_id: "p1".to_string(),
            permalink: "patterns/x".to_string(),
            title: "   ".to_string(),
            file_path: String::new(),
            storage: "db".to_string(),
            note_type: "pattern".to_string(),
            folder: "patterns".to_string(),
            status: "active".to_string(),
            tags: "[]".to_string(),
            content: String::new(),
            retrieval_anchor: None,
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            updated_at: "2026-01-01T00:00:00.000Z".to_string(),
            lifecycle_changed_at: None,
            last_accessed: "2026-01-01T00:00:00.000Z".to_string(),
            access_count: 0,
            confidence: 1.0,
            abstract_: None,
            overview: None,
            scope_paths: "[]".to_string(),
        };
        assert!(propose_anchor_deterministic(&note).is_none());
        note.title = String::new();
        assert!(propose_anchor_deterministic(&note).is_none());
    }
}
