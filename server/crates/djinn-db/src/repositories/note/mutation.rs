//! Atomic, ledger-backed note mutation boundary.
//!
//! This module is intentionally the only place that writes
//! `note_revision_events`. Callers submit a fully specified desired state plus
//! trusted attribution; the repository locks, compares, mutates, sequences, and
//! records the audit event in one PostgreSQL transaction.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{
    NoteAssociationKind, NoteHistoryRequest, NoteRepository, NoteRevisionEventKind,
    NoteRevisionEventRow, NoteRevisionReason, NoteRevisionSnapshot, NoteRevisionSubsystem,
    REVISION_PAGE_MAX, RevisionCursor, RevisionHistoryPage, RevisionLookupRequest,
    RevisionRangeRequest, SessionRevisionPage, SessionRevisionRequest,
    TrustedNoteRevisionAttribution, TrustedNoteRevisionProvenance, index_links_for_note,
    resolve_links_for_note,
};
use crate::error::{DbError as Error, DbResult as Result};
use crate::note_hash::note_content_hash;
use djinn_memory::Note;

use super::scoring::{CONFIDENCE_CEILING, CONFIDENCE_FLOOR};

#[derive(Debug, Clone, PartialEq)]
pub struct NoteSupersedesAssociation {
    pub note_a_id: String,
    pub note_b_id: String,
    pub kind: NoteAssociationKind,
    pub weight: f64,
}

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
    GuardedPatch {
        expected_content: String,
        content: String,
        confidence: f64,
    },
    DeprecateWithSupersedes {
        superseding_note_id: String,
        association_weight: f64,
    },
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
    pub deprecated_note_id: Option<String>,
    pub superseding_note_id: Option<String>,
    pub supersedes_association: Option<NoteSupersedesAssociation>,
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
            NoteRevisionEventKind::Updated => match command.desired {
                NoteRevisionDesiredState::DeprecateWithSupersedes { .. } => {
                    self.deprecate_with_supersedes(&mut tx, &command).await?
                }
                _ => self.update_with_revision(&mut tx, &command).await?,
            },
            NoteRevisionEventKind::ConfidenceChanged => {
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
            deprecated_note_id: None,
            superseding_note_id: None,
            supersedes_association: None,
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
            NoteRevisionDesiredState::GuardedPatch {
                expected_content,
                content,
                confidence,
            } => {
                if before.content != *expected_content {
                    return Err(Error::InvalidData(
                        "guarded patch expected content is stale".to_owned(),
                    ));
                }
                (content, confidence, None)
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
                deprecated_note_id: None,
                superseding_note_id: None,
                supersedes_association: None,
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
            deprecated_note_id: None,
            superseding_note_id: None,
            supersedes_association: None,
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
            deprecated_note_id: None,
            superseding_note_id: None,
            supersedes_association: None,
        })
    }

    async fn deprecate_with_supersedes(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        command: &NoteRevisionMutation,
    ) -> Result<NoteRevisionMutationResult> {
        let old_id = command.note_id.as_deref().expect("validated note identity");
        let NoteRevisionDesiredState::DeprecateWithSupersedes {
            superseding_note_id,
            association_weight,
        } = &command.desired
        else {
            unreachable!("validated deprecation command")
        };
        if old_id == superseding_note_id {
            return Err(Error::InvalidData(
                "superseding note must differ from deprecated note".to_owned(),
            ));
        }
        let (first, second) = if old_id < superseding_note_id.as_str() {
            (old_id, superseding_note_id.as_str())
        } else {
            (superseding_note_id.as_str(), old_id)
        };
        let locked: Vec<Note> = sqlx::query_as("SELECT id, project_id, permalink, title, file_path, storage, note_type, folder, status, tags::text AS tags, content, retrieval_anchor, created_at, updated_at, last_accessed, access_count, confidence, abstract as abstract_, overview, scope_paths::text AS scope_paths FROM notes WHERE project_id = $1 AND id IN ($2, $3) ORDER BY id FOR UPDATE")
            .bind(&command.project_id).bind(first).bind(second).fetch_all(&mut **tx).await?;
        if locked.len() != 2 {
            return Err(Error::InvalidData(
                "deprecated and superseding notes must belong to the command project".to_owned(),
            ));
        }
        let before = locked
            .into_iter()
            .find(|note| note.id == old_id)
            .expect("old note locked");
        sqlx::query("UPDATE notes SET status = 'deprecated', updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1 AND project_id = $2")
            .bind(old_id).bind(&command.project_id).execute(&mut **tx).await?;
        let association = self
            .record_supersedes_in_transaction(tx, superseding_note_id, old_id, *association_weight)
            .await?;
        let note = locked_note(tx, old_id, &command.project_id).await?;
        let seq = next_sequence(tx, &command.project_id, old_id).await?;
        let revision_id = self
            .insert_revision(
                tx,
                command,
                Some(old_id),
                Some(seq),
                Some(&before.content),
                Some(&note.content),
                Some(before.confidence),
                Some(note.confidence),
            )
            .await?;
        Ok(NoteRevisionMutationResult {
            changed: true,
            note: Some(note),
            note_seq: Some(seq),
            revision_id: Some(revision_id),
            deprecated_note_id: Some(old_id.to_owned()),
            superseding_note_id: Some(superseding_note_id.clone()),
            supersedes_association: Some(association),
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
            deprecated_note_id: None,
            superseding_note_id: None,
            supersedes_association: None,
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

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_supersedes_association_failure_for_test(&self, enabled: bool) {
        self.association_failure.store(enabled, Ordering::SeqCst);
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

    // ── Bounded read boundary ────────────────────────────────────────────────

    /// Returns one note's revision history ordered newest-first by
    /// `note_seq`, with stable before-cursor pagination.
    ///
    /// Queries `limit + 1` rows to derive `next_cursor`.  All SQL requires
    /// `project_id` scope.  Deleted-note events are retained.  The
    /// `note_exists` flag distinguishes a live pre-migration note with zero
    /// events from an unknown/deleted note.
    pub async fn note_revision_history(
        &self,
        request: NoteHistoryRequest<'_>,
    ) -> Result<RevisionHistoryPage> {
        self.db.ensure_initialized().await?;
        let limit = request.limit.clamp(1, REVISION_PAGE_MAX);
        let before_seq = match request.before {
            Some(raw) => match RevisionCursor::decode_note_history(raw) {
                Ok(RevisionCursor::NoteHistory { note_seq }) => Some(note_seq),
                Ok(_) => unreachable!("decode_note_history returns NoteHistory"),
                Err(_) => return Err(Error::InvalidData("invalid cursor".to_owned())),
            },
            None => None,
        };
        let fetch = limit + 1;
        let rows: Vec<NoteRevisionDbRow> = if let Some(seq) = before_seq {
            sqlx::query_as(
                "SELECT id, project_id, note_id, note_seq, event_kind, content_before, content_after, \
                 confidence_before, confidence_after, actor_kind, actor_id, subsystem, session_id, \
                 task_id, task_run_id, reason, created_at \
                 FROM note_revision_events \
                 WHERE project_id = $1 AND note_id = $2 AND note_seq < $3 \
                 ORDER BY note_seq DESC, created_at DESC, id DESC \
                 LIMIT $4",
            )
            .bind(request.project_id)
            .bind(request.note_id)
            .bind(seq)
            .bind(fetch as i64)
            .fetch_all(self.db.pool())
            .await?
        } else {
            sqlx::query_as(
                "SELECT id, project_id, note_id, note_seq, event_kind, content_before, content_after, \
                 confidence_before, confidence_after, actor_kind, actor_id, subsystem, session_id, \
                 task_id, task_run_id, reason, created_at \
                 FROM note_revision_events \
                 WHERE project_id = $1 AND note_id = $2 \
                 ORDER BY note_seq DESC, created_at DESC, id DESC \
                 LIMIT $3",
            )
            .bind(request.project_id)
            .bind(request.note_id)
            .bind(fetch as i64)
            .fetch_all(self.db.pool())
            .await?
        };
        let note_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM notes WHERE id = $1 AND project_id = $2)",
        )
        .bind(request.note_id)
        .bind(request.project_id)
        .fetch_one(self.db.pool())
        .await?;
        let next_cursor = if rows.len() > limit {
            let last = &rows[limit - 1];
            Some(RevisionCursor::encode_note_history(
                last.note_seq.unwrap_or(0),
            ))
        } else {
            None
        };
        let events = rows
            .into_iter()
            .take(limit)
            .map(map_db_row_to_event_row)
            .collect::<Result<Vec<_>>>()?;
        Ok(RevisionHistoryPage {
            events,
            next_cursor,
            note_exists,
        })
    }

    /// Returns an explicit single revision, scoped to project and note so
    /// inaccessible or cross-project IDs are indistinguishable to callers.
    ///
    /// Returns `Ok(None)` when the revision does not exist within the scoped
    /// project/note pair.
    pub async fn revision_lookup(
        &self,
        request: RevisionLookupRequest<'_>,
    ) -> Result<Option<NoteRevisionEventRow>> {
        self.db.ensure_initialized().await?;
        let row: Option<NoteRevisionDbRow> = sqlx::query_as(
            "SELECT id, project_id, note_id, note_seq, event_kind, content_before, content_after, \
             confidence_before, confidence_after, actor_kind, actor_id, subsystem, session_id, \
             task_id, task_run_id, reason, created_at \
             FROM note_revision_events \
             WHERE project_id = $1 AND note_id = $2 AND id = $3",
        )
        .bind(request.project_id)
        .bind(request.note_id)
        .bind(request.revision_id)
        .fetch_optional(self.db.pool())
        .await?;
        row.map(map_db_row_to_event_row).transpose()
    }

    /// Returns an inclusive revision range between two `note_seq` endpoints,
    /// ordered newest-first.  Both endpoints and all intervening events are
    /// included regardless of event kind, so callers can build deterministic
    /// pairwise diffs from explicit snapshots without inferring neighboring
    /// state.
    pub async fn revision_range(
        &self,
        request: RevisionRangeRequest<'_>,
    ) -> Result<Vec<NoteRevisionEventRow>> {
        self.db.ensure_initialized().await?;
        let (to_seq, from_seq) = if request.to_seq >= request.from_seq {
            (request.to_seq, request.from_seq)
        } else {
            (request.from_seq, request.to_seq)
        };
        let rows: Vec<NoteRevisionDbRow> = sqlx::query_as(
            "SELECT id, project_id, note_id, note_seq, event_kind, content_before, content_after, \
             confidence_before, confidence_after, actor_kind, actor_id, subsystem, session_id, \
             task_id, task_run_id, reason, created_at \
             FROM note_revision_events \
             WHERE project_id = $1 AND note_id = $2 AND note_seq BETWEEN $3 AND $4 \
             ORDER BY note_seq DESC, created_at DESC, id DESC",
        )
        .bind(request.project_id)
        .bind(request.note_id)
        .bind(from_seq)
        .bind(to_seq)
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter()
            .map(map_db_row_to_event_row)
            .collect::<Result<Vec<_>>>()
    }

    /// Returns one session's revision events ordered newest-first by
    /// `(created_at DESC, id DESC)`, with stable before-cursor pagination.
    /// Includes every ledger event kind.
    pub async fn session_revision_history(
        &self,
        session_id: &str,
        request: SessionRevisionRequest<'_>,
    ) -> Result<SessionRevisionPage> {
        self.session_or_task_run_history("session_id", session_id, request)
            .await
    }

    /// Returns one task-run's revision events ordered newest-first by
    /// `(created_at DESC, id DESC)`, with stable before-cursor pagination.
    /// Includes every ledger event kind.
    pub async fn task_run_revision_history(
        &self,
        task_run_id: &str,
        request: SessionRevisionRequest<'_>,
    ) -> Result<SessionRevisionPage> {
        self.session_or_task_run_history("task_run_id", task_run_id, request)
            .await
    }

    async fn session_or_task_run_history(
        &self,
        scope_column: &'static str,
        scope_value: &str,
        request: SessionRevisionRequest<'_>,
    ) -> Result<SessionRevisionPage> {
        self.db.ensure_initialized().await?;
        let limit = request.limit.clamp(1, REVISION_PAGE_MAX);
        let before = match request.before {
            Some(raw) => match RevisionCursor::decode_session(raw) {
                Ok(RevisionCursor::Session { created_at, id }) => Some((created_at, id)),
                Ok(_) => unreachable!("decode_session returns Session"),
                Err(_) => return Err(Error::InvalidData("invalid cursor".to_owned())),
            },
            None => None,
        };
        let fetch = limit + 1;
        // The scope column is a compile-time constant, not user input, so
        // interpolation into the SQL string is safe.
        let rows: Vec<NoteRevisionDbRow> = if let Some((cursor_created_at, cursor_id)) = before {
            let sql = format!(
                "SELECT id, project_id, note_id, note_seq, event_kind, content_before, content_after, \
                 confidence_before, confidence_after, actor_kind, actor_id, subsystem, session_id, \
                 task_id, task_run_id, reason, created_at \
                 FROM note_revision_events \
                 WHERE project_id = $1 AND {scope_column} = $2 \
                 AND (created_at, id) < ($3, $4) \
                 ORDER BY created_at DESC, id DESC \
                 LIMIT $5"
            );
            sqlx::query_as(&sql)
                .bind(request.project_id)
                .bind(scope_value)
                .bind(cursor_created_at)
                .bind(cursor_id)
                .bind(fetch as i64)
                .fetch_all(self.db.pool())
                .await?
        } else {
            let sql = format!(
                "SELECT id, project_id, note_id, note_seq, event_kind, content_before, content_after, \
                 confidence_before, confidence_after, actor_kind, actor_id, subsystem, session_id, \
                 task_id, task_run_id, reason, created_at \
                 FROM note_revision_events \
                 WHERE project_id = $1 AND {scope_column} = $2 \
                 ORDER BY created_at DESC, id DESC \
                 LIMIT $3"
            );
            sqlx::query_as(&sql)
                .bind(request.project_id)
                .bind(scope_value)
                .bind(fetch as i64)
                .fetch_all(self.db.pool())
                .await?
        };
        let next_cursor = if rows.len() > limit {
            let last = &rows[limit - 1];
            Some(RevisionCursor::encode_session(&last.created_at, &last.id))
        } else {
            None
        };
        let events = rows
            .into_iter()
            .take(limit)
            .map(map_db_row_to_event_row)
            .collect::<Result<Vec<_>>>()?;
        Ok(SessionRevisionPage {
            events,
            next_cursor,
        })
    }
}

// ── Read-row mapping ─────────────────────────────────────────────────────────

/// Raw DB row shape matching the physical `note_revision_events` columns.
#[derive(Debug, Clone, sqlx::FromRow)]
struct NoteRevisionDbRow {
    id: String,
    project_id: String,
    note_id: Option<String>,
    note_seq: Option<i64>,
    event_kind: String,
    content_before: Option<String>,
    content_after: Option<String>,
    confidence_before: Option<f64>,
    confidence_after: Option<f64>,
    actor_kind: String,
    actor_id: Option<String>,
    subsystem: Option<String>,
    session_id: Option<String>,
    task_id: Option<String>,
    task_run_id: Option<String>,
    reason: String,
    created_at: String,
}

/// Maps a raw DB row to the typed [`NoteRevisionEventRow`], reconstructing the
/// trusted attribution/provenance and validated reason from persisted values.
fn map_db_row_to_event_row(row: NoteRevisionDbRow) -> Result<NoteRevisionEventRow> {
    let event_kind = parse_event_kind(&row.event_kind)?;
    let attribution = reconstruct_attribution(&row)?;
    let provenance = TrustedNoteRevisionProvenance::new(
        row.session_id.clone(),
        row.task_id.clone(),
        row.task_run_id.clone(),
    )
    .map_err(|_| Error::Internal("invalid persisted provenance".to_owned()))?;
    let reason = NoteRevisionReason::new(&row.reason)
        .map_err(|_| Error::Internal(format!("invalid persisted reason: {}", row.reason)))?;
    Ok(NoteRevisionEventRow {
        id: row.id,
        project_id: row.project_id,
        note_id: row.note_id,
        note_seq: row.note_seq,
        event_kind,
        snapshot: NoteRevisionSnapshot {
            content_before: row.content_before,
            content_after: row.content_after,
            confidence_before: row.confidence_before,
            confidence_after: row.confidence_after,
        },
        attribution,
        provenance,
        reason,
        created_at: row.created_at,
    })
}

fn parse_event_kind(raw: &str) -> Result<NoteRevisionEventKind> {
    Ok(match raw {
        "created" => NoteRevisionEventKind::Created,
        "updated" => NoteRevisionEventKind::Updated,
        "deleted" => NoteRevisionEventKind::Deleted,
        "confidence_changed" => NoteRevisionEventKind::ConfidenceChanged,
        "extraction_skipped" => NoteRevisionEventKind::ExtractionSkipped,
        other => {
            return Err(Error::Internal(format!(
                "unknown persisted event kind: {other}"
            )));
        }
    })
}

fn reconstruct_attribution(row: &NoteRevisionDbRow) -> Result<TrustedNoteRevisionAttribution> {
    Ok(match row.actor_kind.as_str() {
        "human" => {
            TrustedNoteRevisionAttribution::human(row.actor_id.as_deref().unwrap_or_default())
                .map_err(|_| Error::Internal("invalid persisted human attribution".to_owned()))?
        }
        "agent" => {
            TrustedNoteRevisionAttribution::agent(row.actor_id.as_deref().unwrap_or_default())
                .map_err(|_| Error::Internal("invalid persisted agent attribution".to_owned()))?
        }
        "system" => {
            let subsystem = row.subsystem.as_deref().ok_or_else(|| {
                Error::Internal("system attribution missing subsystem".to_owned())
            })?;
            let subsystem_enum = match subsystem {
                "mcp" => NoteRevisionSubsystem::Mcp,
                "dedup" => NoteRevisionSubsystem::Dedup,
                "consolidation" => NoteRevisionSubsystem::Consolidation,
                "enrichment" => NoteRevisionSubsystem::Enrichment,
                "extraction" => NoteRevisionSubsystem::Extraction,
                other => {
                    return Err(Error::Internal(format!(
                        "unknown persisted subsystem: {other}"
                    )));
                }
            };
            TrustedNoteRevisionAttribution::system(subsystem_enum)
        }
        other => {
            return Err(Error::Internal(format!(
                "unknown persisted actor kind: {other}"
            )));
        }
    })
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
            NoteRevisionDesiredState::GuardedPatch { .. }
        ) | (
            NoteRevisionEventKind::Updated,
            NoteRevisionDesiredState::ExistingWithMetadata(_)
        ) | (
            NoteRevisionEventKind::Updated,
            NoteRevisionDesiredState::DeprecateWithSupersedes { .. }
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
    if let NoteRevisionDesiredState::GuardedPatch { confidence, .. } = &command.desired
        && (!confidence.is_finite()
            || !(CONFIDENCE_FLOOR..=CONFIDENCE_CEILING).contains(confidence))
    {
        return Err(Error::InvalidData(format!(
            "guarded patch confidence must be within [{CONFIDENCE_FLOOR}, {CONFIDENCE_CEILING}]"
        )));
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
