//! Atomic, ledger-backed note mutation boundary.
//!
//! This module is intentionally the only place that writes
//! `note_revision_events`. Callers submit a fully specified desired state plus
//! trusted attribution; the repository locks, compares, mutates, sequences, and
//! records the audit event in one PostgreSQL transaction.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    NoteRepository, NoteRevisionEventKind, NoteRevisionReason, TrustedNoteRevisionAttribution,
    TrustedNoteRevisionProvenance, index_links_for_note, resolve_links_for_note,
};
use crate::error::{DbError as Error, DbResult as Result};
use crate::note_hash::note_content_hash;
use djinn_memory::Note;

/// Immutable revision row returned by the production query surface.
///
/// This mirrors the ledger columns so callers can verify authorization without
/// bypassing the repository boundary with direct SQL.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct NoteRevisionEvent {
    pub id: String,
    pub project_id: String,
    pub note_id: Option<String>,
    pub note_seq: Option<i64>,
    pub actor_kind: String,
    pub actor_id: Option<String>,
    pub subsystem: Option<String>,
    pub event_kind: String,
    pub content_before: Option<String>,
    pub content_after: Option<String>,
    pub confidence_before: Option<f64>,
    pub confidence_after: Option<f64>,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub task_run_id: Option<String>,
    pub reason: String,
    pub created_at: String,
}

/// Canonical final values for an MCP edit, including optional move metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteRevisionUpdateState {
    pub title: String,
    pub permalink: String,
    pub content: String,
    pub note_type: String,
    pub folder: String,
    pub tags: String,
    pub retrieval_anchor: Option<String>,
    pub confidence: f64,
}

/// Canonical final values for a created note. Updates intentionally use only
/// `content` and `confidence`: existing public CRUD APIs retain ownership of
/// the unrelated legacy note fields until their callers migrate.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteRevisionCreateState {
    pub title: String,
    pub permalink: String,
    pub content: String,
    pub note_type: String,
    pub folder: String,
    pub status: String,
    pub tags: String,
    pub retrieval_anchor: Option<String>,
    pub scope_paths: String,
    pub confidence: f64,
}

/// The requested final state for a ledger-backed mutation.
#[derive(Debug, Clone, PartialEq)]
pub enum NoteRevisionDesiredState {
    Create(NoteRevisionCreateState),
    /// Replace the canonical persisted content and confidence of an existing
    /// note. Equality is evaluated after the row has been locked.
    Existing {
        content: String,
        confidence: f64,
    },
    ExistingWithMetadata(NoteRevisionUpdateState),
    /// Delete the locked note after retaining its before snapshot in the ledger.
    Delete,
    /// Record an extraction run which intentionally produced no note.
    ExtractionSkipped,
}

/// The single public command accepted by the transactional revision boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteRevisionMutation {
    pub project_id: String,
    /// Stable note UUID. It is absent only for `ExtractionSkipped`.
    pub note_id: Option<String>,
    pub event_kind: NoteRevisionEventKind,
    pub desired: NoteRevisionDesiredState,
    pub attribution: TrustedNoteRevisionAttribution,
    pub provenance: TrustedNoteRevisionProvenance,
    pub reason: NoteRevisionReason,
}

/// The committed outcome of a [`NoteRevisionMutation`].
#[derive(Debug, Clone)]
pub struct NoteRevisionMutationResult {
    pub changed: bool,
    pub note: Option<Note>,
    pub note_seq: Option<i64>,
    pub revision_id: Option<String>,
}

impl NoteRepository {
    /// Atomically applies a note mutation and appends its immutable revision.
    ///
    /// This owns the transaction boundary. In particular there is deliberately
    /// no public standalone revision insertion operation.
    pub async fn mutate_with_revision(
        &self,
        command: NoteRevisionMutation,
    ) -> Result<NoteRevisionMutationResult> {
        self.db.ensure_initialized().await?;
        validate_command(&command)?;

        let event_kind = command.event_kind;
        let note_id = command.note_id.clone();
        let mut tx = self.db.pool().begin().await?;
        let result = match command.event_kind {
            NoteRevisionEventKind::ExtractionSkipped => {
                self.insert_skipped(&mut tx, &command).await?
            }
            NoteRevisionEventKind::Created => self.create_with_revision(&mut tx, &command).await?,
            NoteRevisionEventKind::Updated | NoteRevisionEventKind::ConfidenceChanged => {
                self.update_with_revision(&mut tx, &command).await?
            }
            NoteRevisionEventKind::Deleted => self.delete_with_revision(&mut tx, &command).await?,
        };
        tx.commit().await?;
        if result.changed {
            match event_kind {
                NoteRevisionEventKind::Created => {
                    if let Some(note) = result.note.as_ref() {
                        self.spawn_note_embedding_sync(note);
                        self.events.send(djinn_memory::events::note_created(note));
                    }
                }
                NoteRevisionEventKind::Updated | NoteRevisionEventKind::ConfidenceChanged => {
                    if let Some(note) = result.note.as_ref() {
                        self.spawn_note_embedding_sync(note);
                        self.events.send(djinn_memory::events::note_updated(note));
                    }
                }
                NoteRevisionEventKind::Deleted => {
                    if let Some(note_id) = note_id.as_deref() {
                        if let Err(error) = self.delete_embedding(note_id).await {
                            tracing::warn!(%note_id, %error, "failed to delete note embedding during revision-backed removal");
                        }
                        self.events
                            .send(djinn_memory::events::note_deleted(note_id));
                    }
                }
                NoteRevisionEventKind::ExtractionSkipped => {}
            }
        }
        Ok(result)
    }

    async fn create_with_revision(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        command: &NoteRevisionMutation,
    ) -> Result<NoteRevisionMutationResult> {
        let note_id = command.note_id.as_deref().expect("validated note identity");
        // The transaction-scoped lock also serializes two creators before there
        // is a row available for `FOR UPDATE`.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("{}:{note_id}", command.project_id))
            .execute(&mut **tx)
            .await?;
        let existing: Option<String> =
            sqlx::query_scalar("SELECT id FROM notes WHERE id = $1 AND project_id = $2 FOR UPDATE")
                .bind(note_id)
                .bind(&command.project_id)
                .fetch_optional(&mut **tx)
                .await?;
        if existing.is_some() {
            return Err(Error::InvalidData(format!(
                "note already exists: {note_id}"
            )));
        }
        let historical: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM note_revision_events WHERE project_id = $1 AND note_id = $2",
        )
        .bind(&command.project_id)
        .bind(note_id)
        .fetch_one(&mut **tx)
        .await?;
        if historical != 0 {
            return Err(Error::InvalidData(format!(
                "cannot recreate note identity with retained revision history: {note_id}"
            )));
        }

        let NoteRevisionDesiredState::Create(desired) = &command.desired else {
            unreachable!("validated create command")
        };
        let tags: serde_json::Value = serde_json::from_str(&desired.tags)
            .map_err(|e| Error::InvalidData(format!("invalid json for notes.tags: {e}")))?;
        let scope_paths: serde_json::Value = serde_json::from_str(&desired.scope_paths)
            .map_err(|e| Error::InvalidData(format!("invalid json for notes.scope_paths: {e}")))?;
        sqlx::query(
            "INSERT INTO notes (id, project_id, permalink, title, file_path, storage, note_type, folder, status, tags, content, retrieval_anchor, content_hash, scope_paths, confidence) \
             VALUES ($1, $2, $3, $4, '', 'db', $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(note_id)
        .bind(&command.project_id)
        .bind(&desired.permalink)
        .bind(&desired.title)
        .bind(&desired.note_type)
        .bind(&desired.folder)
        .bind(&desired.status)
        .bind(tags)
        .bind(&desired.content)
        .bind(&desired.retrieval_anchor)
        .bind(note_content_hash(&desired.content))
        .bind(scope_paths)
        .bind(desired.confidence)
        .execute(&mut **tx)
        .await?;
        index_links_for_note(tx, note_id, &command.project_id, &desired.content).await?;
        resolve_links_for_note(
            tx,
            note_id,
            &desired.title,
            &desired.permalink,
            &command.project_id,
        )
        .await?;
        let note = locked_note(tx, note_id, &command.project_id).await?;
        let revision_id = self
            .insert_revision(
                tx,
                command,
                Some(note_id),
                Some(1),
                None,
                Some(&note.content),
                None,
                Some(note.confidence),
            )
            .await?;
        Ok(NoteRevisionMutationResult {
            changed: true,
            note: Some(note),
            note_seq: Some(1),
            revision_id: Some(revision_id),
        })
    }

    async fn update_with_revision(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        command: &NoteRevisionMutation,
    ) -> Result<NoteRevisionMutationResult> {
        let note_id = command.note_id.as_deref().expect("validated note identity");
        let before = locked_note(tx, note_id, &command.project_id).await?;
        let (content, confidence, metadata) = match &command.desired {
            NoteRevisionDesiredState::Existing {
                content,
                confidence,
            } => (content, confidence, None),
            NoteRevisionDesiredState::ExistingWithMetadata(state) => {
                (&state.content, &state.confidence, Some(state))
            }
            _ => unreachable!("validated update command"),
        };
        let metadata_changed = metadata.is_some_and(|state| {
            before.title != state.title
                || before.permalink != state.permalink
                || before.note_type != state.note_type
                || before.folder != state.folder
                || before.tags != state.tags
                || before.retrieval_anchor != state.retrieval_anchor
        });
        if before.content == *content && before.confidence == *confidence && !metadata_changed {
            return Ok(NoteRevisionMutationResult {
                changed: false,
                note: Some(before),
                note_seq: None,
                revision_id: None,
            });
        }
        if command.event_kind == NoteRevisionEventKind::ConfidenceChanged
            && before.content != *content
        {
            return Err(Error::InvalidData(
                "confidence_changed must not alter content".to_owned(),
            ));
        }
        if let Some(state) = metadata {
            let tags: serde_json::Value = serde_json::from_str(&state.tags)
                .map_err(|e| Error::InvalidData(format!("invalid json for notes.tags: {e}")))?;
            sqlx::query("UPDATE notes SET title = $1, permalink = $2, content = $3, note_type = $4, folder = $5, tags = $6, retrieval_anchor = $7, confidence = $8, content_hash = $9, updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $10 AND project_id = $11")
                .bind(&state.title).bind(&state.permalink).bind(content).bind(&state.note_type)
                .bind(&state.folder).bind(tags).bind(&state.retrieval_anchor).bind(confidence)
                .bind(note_content_hash(content)).bind(note_id).bind(&command.project_id)
                .execute(&mut **tx).await?;
        } else {
            sqlx::query("UPDATE notes SET content = $1, confidence = $2, content_hash = $3, updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $4 AND project_id = $5")
                .bind(content).bind(confidence).bind(note_content_hash(content)).bind(note_id)
                .bind(&command.project_id).execute(&mut **tx).await?;
        }
        // Link replacement belongs to the same transaction as every content
        // mutation, including metadata-free writers such as extraction.
        // This removes stale outbound rows and makes newly authored links
        // visible before the note revision commits.
        index_links_for_note(tx, note_id, &command.project_id, content).await?;
        let link_title = metadata.map_or(&before.title, |state| &state.title);
        let link_permalink = metadata.map_or(&before.permalink, |state| &state.permalink);
        resolve_links_for_note(tx, note_id, link_title, link_permalink, &command.project_id)
            .await?;
        let note = locked_note(tx, note_id, &command.project_id).await?;
        let seq = next_sequence(tx, &command.project_id, note_id).await?;
        let (content_before, content_after) =
            if command.event_kind == NoteRevisionEventKind::ConfidenceChanged {
                (None, None)
            } else {
                (Some(before.content.as_str()), Some(note.content.as_str()))
            };
        let revision_id = self
            .insert_revision(
                tx,
                command,
                Some(note_id),
                Some(seq),
                content_before,
                content_after,
                Some(before.confidence),
                Some(note.confidence),
            )
            .await?;
        Ok(NoteRevisionMutationResult {
            changed: true,
            note: Some(note),
            note_seq: Some(seq),
            revision_id: Some(revision_id),
        })
    }

    async fn delete_with_revision(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        command: &NoteRevisionMutation,
    ) -> Result<NoteRevisionMutationResult> {
        let note_id = command.note_id.as_deref().expect("validated note identity");
        let before = locked_note(tx, note_id, &command.project_id).await?;
        let seq = next_sequence(tx, &command.project_id, note_id).await?;
        let revision_id = self
            .insert_revision(
                tx,
                command,
                Some(note_id),
                Some(seq),
                Some(&before.content),
                None,
                Some(before.confidence),
                None,
            )
            .await?;
        sqlx::query("DELETE FROM notes WHERE id = $1 AND project_id = $2")
            .bind(note_id)
            .bind(&command.project_id)
            .execute(&mut **tx)
            .await?;
        Ok(NoteRevisionMutationResult {
            changed: true,
            note: None,
            note_seq: Some(seq),
            revision_id: Some(revision_id),
        })
    }

    async fn insert_skipped(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        command: &NoteRevisionMutation,
    ) -> Result<NoteRevisionMutationResult> {
        let revision_id = self
            .insert_revision(tx, command, None, None, None, None, None, None)
            .await?;
        Ok(NoteRevisionMutationResult {
            changed: true,
            note: None,
            note_seq: None,
            revision_id: Some(revision_id),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_revision(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        command: &NoteRevisionMutation,
        note_id: Option<&str>,
        note_seq: Option<i64>,
        content_before: Option<&str>,
        content_after: Option<&str>,
        confidence_before: Option<f64>,
        confidence_after: Option<f64>,
    ) -> Result<String> {
        if self.revision_event_failure.load(Ordering::SeqCst) {
            return Err(Error::Internal(
                "forced note revision event insertion failure".to_owned(),
            ));
        }
        let id = uuid::Uuid::now_v7().to_string();
        sqlx::query("INSERT INTO note_revision_events (id, project_id, note_id, note_seq, event_kind, content_before, content_after, confidence_before, confidence_after, actor_kind, actor_id, subsystem, session_id, task_id, task_run_id, reason) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)")
            .bind(&id).bind(&command.project_id).bind(note_id).bind(note_seq).bind(command.event_kind.as_str()).bind(content_before).bind(content_after).bind(confidence_before).bind(confidence_after).bind(command.attribution.actor_kind().as_str()).bind(command.attribution.actor_id()).bind(command.attribution.subsystem()).bind(command.provenance.session_id()).bind(command.provenance.task_id()).bind(command.provenance.task_run_id()).bind(command.reason.as_str()).execute(&mut **tx).await?;
        Ok(id)
    }

    /// Test-only deterministic seam for proving transaction rollback. It is not
    /// compiled into normal production consumers.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_revision_event_insertion_failure_for_test(&self, enabled: bool) {
        self.revision_event_failure.store(enabled, Ordering::SeqCst);
    }

    /// Returns immutable revision rows for one project in creation order.
    pub async fn revision_events(&self, project_id: &str) -> Result<Vec<NoteRevisionEvent>> {
        sqlx::query_as(
            "SELECT id, project_id, note_id, note_seq, actor_kind, actor_id, subsystem, \
             event_kind, content_before, content_after, confidence_before, confidence_after, \
             session_id, task_id, task_run_id, reason, created_at::text AS created_at \
             FROM note_revision_events WHERE project_id = $1 ORDER BY created_at, id",
        )
        .bind(project_id)
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }

    /// Returns immutable revision rows for one note, scoped to its project.
    pub async fn revision_events_for_note(
        &self,
        project_id: &str,
        note_id: &str,
    ) -> Result<Vec<NoteRevisionEvent>> {
        sqlx::query_as(
            "SELECT id, project_id, note_id, note_seq, actor_kind, actor_id, subsystem, \
             event_kind, content_before, content_after, confidence_before, confidence_after, \
             session_id, task_id, task_run_id, reason, created_at::text AS created_at \
             FROM note_revision_events WHERE project_id = $1 AND note_id = $2 \
             ORDER BY note_seq, created_at, id",
        )
        .bind(project_id)
        .bind(note_id)
        .fetch_all(self.db.pool())
        .await
        .map_err(Into::into)
    }
}

async fn locked_note(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    note_id: &str,
    project_id: &str,
) -> Result<Note> {
    sqlx::query_as::<_, Note>("SELECT id, project_id, permalink, title, file_path, storage, note_type, folder, status, tags::text AS tags, content, retrieval_anchor, created_at, updated_at, last_accessed, access_count, confidence, abstract as abstract_, overview, scope_paths::text AS scope_paths FROM notes WHERE id = $1 AND project_id = $2 FOR UPDATE")
        .bind(note_id).bind(project_id).fetch_optional(&mut **tx).await?
        .ok_or_else(|| Error::InvalidData(format!("note not found: {note_id}")))
}

async fn next_sequence(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    project_id: &str,
    note_id: &str,
) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT COALESCE(MAX(note_seq), 0) + 1 FROM note_revision_events WHERE project_id = $1 AND note_id = $2")
        .bind(project_id).bind(note_id).fetch_one(&mut **tx).await?)
}

fn validate_command(command: &NoteRevisionMutation) -> Result<()> {
    let note_required = !matches!(command.event_kind, NoteRevisionEventKind::ExtractionSkipped);
    if note_required != command.note_id.is_some() {
        return Err(Error::InvalidData(
            "note identity must be present exactly for note revision events".to_owned(),
        ));
    }
    let valid = matches!(
        (&command.event_kind, &command.desired),
        (
            NoteRevisionEventKind::Created,
            NoteRevisionDesiredState::Create(_)
        ) | (
            NoteRevisionEventKind::Updated | NoteRevisionEventKind::ConfidenceChanged,
            NoteRevisionDesiredState::Existing { .. }
        ) | (
            NoteRevisionEventKind::Updated,
            NoteRevisionDesiredState::ExistingWithMetadata(_)
        ) | (
            NoteRevisionEventKind::Deleted,
            NoteRevisionDesiredState::Delete
        ) | (
            NoteRevisionEventKind::ExtractionSkipped,
            NoteRevisionDesiredState::ExtractionSkipped
        )
    );
    if !valid {
        return Err(Error::InvalidData(
            "event kind does not match desired final state".to_owned(),
        ));
    }
    if command.event_kind == NoteRevisionEventKind::ExtractionSkipped
        && command.provenance.session_id().is_none()
        && command.provenance.task_run_id().is_none()
    {
        return Err(Error::InvalidData(
            "extraction_skipped requires trusted session or task-run provenance".to_owned(),
        ));
    }
    Ok(())
}

// Keep these imports live in production even though the test-only seam is cfg'd.
pub(super) fn revision_failure_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
