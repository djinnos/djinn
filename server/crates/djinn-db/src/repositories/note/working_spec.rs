//! Task-scoped working specifications (proposal `t5rn`, T4).
//!
//! # The design decision
//!
//! Working specs stay `note_type = 'design'` and are **archived on terminal task
//! state**. No new note status is introduced. What distinguishes a pipeline
//! working spec from a hand-authored `design` note is machine marking — the
//! reserved [`WORKING_SPEC_TAG`] plus trusted revision provenance carrying
//! `task_id`, `task_run_id`, `session_id`, and extraction subsystem attribution
//! — never a heuristic over title or content.
//!
//! # The two races, and the one lock that closes both
//!
//! Persistence and task termination can interleave:
//!
//! * **terminal-before-first-insert** — the task closes, *then* an in-flight
//!   extraction persists a working spec for the first time. Without
//!   serialization the new note lands `active` under a closed task and nothing
//!   ever archives it, because the terminal transition already ran.
//! * **terminal-racing-reactivation** — a reopened task's persistence and a
//!   fresh terminal transition overlap; the persistence can reactivate a note
//!   the terminal transition has already archived.
//!
//! Both paths take a `FOR UPDATE` lock on **the same task row**:
//!
//! * [`NoteRepository::persist_task_working_spec`] locks the task, reads its
//!   *locked* status, and derives the note's final status from it — `active`
//!   only when the task is non-terminal, `archived` otherwise. It may still
//!   persist the content either way.
//! * The terminal transition (`task::status`) locks the same row, commits the
//!   terminal state, and archives every matching active working spec in the
//!   same transaction.
//!
//! So whichever wins the lock, the loser observes the winner's committed state:
//! a terminal transition that goes first forces a later first insert to be
//! `archived`, and a persistence that goes first is visible to the later
//! terminal transaction and is archived there. Neither ordering can leave an
//! active working spec attached to a terminal task.
//!
//! Reopen deliberately does **not** eagerly reactivate. It only moves the task
//! to a non-terminal status; the next successful persistence locks the task and
//! transitions that same canonical permalink back to `active` before appending.
//!
//! Terminality comes from [`TaskStatus::is_terminal`], which is pinned to the
//! transition table by a test in `djinn-core`, so no separate close-event
//! interpretation exists here.
//!
//! # Known boundary: canonical-permalink adoption
//!
//! Persistence is keyed on the canonical permalink, so a hand-authored `design`
//! note that slugifies onto `design/working-spec-{short_id}` is adopted and
//! thereafter treated as that task's working spec. `slugify` lowercases, so a
//! differently-cased title collides too. This is **pre-existing** behaviour —
//! the previous implementation also looked the note up by permalink and updated
//! it — and is deliberately left unchanged here rather than widened into a scope
//! this proposal did not ask for. It is asserted by
//! `human_note_occupying_the_canonical_permalink_is_adopted_as_pre_existing_behaviour`
//! so any future change to it is a visible decision.
//!
//! Note this is distinct from what the archive path guarantees: a hand-authored
//! design note at *any other* permalink can never be archived by a task
//! transition, because archival matches on machine marking only.

use djinn_core::models::TaskStatus;
use sqlx::Row;

use super::{CONFIDENCE_CEILING, CONFIDENCE_FLOOR, NoteRepository};
use super::{
    NoteRevisionReason, NoteRevisionSubsystem, folder_for_type, index_links_for_note,
    is_consolidation_eligible_note_type, permalink_for, resolve_links_for_note,
};
use crate::error::{DbError as Error, DbResult as Result};
use crate::note_hash::note_content_hash;

/// Reserved machine marker for a pipeline-authored working spec.
pub const WORKING_SPEC_TAG: &str = "working-spec";

/// The exact generated sentence every pipeline working-spec document contains.
///
/// The legacy migration requires this verbatim, so it is defined here and
/// consumed by the renderer rather than being restated at each site.
pub const WORKING_SPEC_CONSTRAINT_SENTENCE: &str =
    "This note is task-scoped working context routed from non-durable extraction output.";

/// Canonical working-spec title for a task short id.
pub fn working_spec_title(task_short_id: &str) -> String {
    format!("Working Spec {task_short_id}")
}

/// Canonical working-spec permalink for a task short id.
pub fn working_spec_permalink(task_short_id: &str) -> String {
    permalink_for("design", &working_spec_title(task_short_id))
}

/// Inputs for one task-lock-serialized working-spec persistence.
#[derive(Debug, Clone)]
pub struct PersistWorkingSpecRequest<'a> {
    pub project_id: &'a str,
    pub task_id: &'a str,
    pub task_short_id: &'a str,
    pub session_id: &'a str,
    pub task_run_id: Option<&'a str>,
    pub scope_paths: &'a str,
    pub reason: NoteRevisionReason,
}

/// Committed outcome of a working-spec persistence.
#[derive(Debug, Clone, PartialEq)]
pub struct PersistedWorkingSpec {
    pub note_id: String,
    pub permalink: String,
    /// Final persisted status, derived from the *locked* task state.
    pub status: String,
    pub created: bool,
    pub changed: bool,
    /// The task status observed under the row lock.
    pub locked_task_status: String,
    pub task_terminal: bool,
    pub revision_id: Option<String>,
}

/// A durable note promoted out of a working-spec section.
#[derive(Debug, Clone, PartialEq)]
pub struct PromotedWorkingSpecNote {
    pub note_id: String,
    pub permalink: String,
    pub creation_revision_id: String,
    /// Provenance copied from the working spec's own trusted revision.
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub task_run_id: Option<String>,
    pub derived_from_note_id: String,
}

/// Inputs for an explicit promotion.
#[derive(Debug, Clone)]
pub struct PromoteWorkingSpecSection<'a> {
    pub project_id: &'a str,
    /// The working spec the section is copied out of. It is left unchanged.
    pub working_spec_note_id: &'a str,
    pub note_type: &'a str,
    pub title: &'a str,
    /// The rewritten, durable prose. Never the raw section verbatim.
    pub content: &'a str,
    pub scope_paths: &'a str,
    pub confidence: f64,
    pub reason: NoteRevisionReason,
}

impl NoteRepository {
    /// Persist a task working spec, serialized on the task row.
    ///
    /// `render` builds the document from the existing content (`None` on first
    /// insert), keeping prose rendering with the caller while this owns the
    /// transaction. It is called **inside** the transaction, after the task lock
    /// is held, so the content it produces always corresponds to the state the
    /// final status was derived from.
    pub async fn persist_task_working_spec(
        &self,
        request: PersistWorkingSpecRequest<'_>,
        render: &(dyn for<'r> Fn(Option<&'r str>) -> String + Send + Sync),
    ) -> Result<PersistedWorkingSpec> {
        self.db.ensure_initialized().await?;
        let permalink = working_spec_permalink(request.task_short_id);
        let title = working_spec_title(request.task_short_id);

        let mut tx = self.db.pool().begin().await?;

        // ── The lock ────────────────────────────────────────────────────────
        // Same row, same mode as the terminal transition. Everything below
        // reads a task state that cannot change until this transaction ends.
        let locked_status: Option<String> =
            sqlx::query_scalar("SELECT status FROM tasks WHERE id = $1 FOR UPDATE")
                .bind(request.task_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(locked_status) = locked_status else {
            tx.rollback().await?;
            return Err(Error::InvalidData(format!(
                "working spec task not found: {}",
                request.task_id
            )));
        };
        let task_terminal = TaskStatus::parse(&locked_status)?.is_terminal();
        // A terminal task may still receive content; what it may not receive is
        // an *active* note.
        let final_status = if task_terminal { "archived" } else { "active" };

        let existing = sqlx::query(
            "SELECT id, content, status FROM notes \
             WHERE project_id = $1 AND permalink = $2 FOR UPDATE",
        )
        .bind(request.project_id)
        .bind(&permalink)
        .fetch_optional(&mut *tx)
        .await?;

        let outcome = match existing {
            Some(row) => {
                let note_id: String = row.try_get("id")?;
                let previous_content: String = row.try_get("content")?;
                let previous_status: String = row.try_get("status")?;
                let content = render(Some(previous_content.as_str()));
                let changed = content != previous_content || previous_status != final_status;

                if !changed {
                    tx.commit().await?;
                    return Ok(PersistedWorkingSpec {
                        note_id,
                        permalink,
                        status: final_status.to_owned(),
                        created: false,
                        changed: false,
                        locked_task_status: locked_status,
                        task_terminal,
                        revision_id: None,
                    });
                }

                // The reserved tag is (re)asserted idempotently: a working spec
                // that predates the marker gains it here without disturbing any
                // other tag the note carries.
                sqlx::query(
                    r#"UPDATE notes
                       SET content = $1,
                           content_hash = $2,
                           status = $3,
                           tags = CASE WHEN tags @> $4::jsonb THEN tags ELSE tags || $4::jsonb END,
                           lifecycle_changed_at = CASE
                               WHEN status <> $3 THEN to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                               ELSE lifecycle_changed_at
                           END,
                           updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
                       WHERE id = $5"#,
                )
                .bind(&content)
                .bind(note_content_hash(&content))
                .bind(final_status)
                .bind(working_spec_tag_json())
                .bind(&note_id)
                .execute(&mut *tx)
                .await?;
                index_links_for_note(&mut tx, &note_id, request.project_id, &content).await?;
                resolve_links_for_note(&mut tx, &note_id, &title, &permalink, request.project_id)
                    .await?;

                let revision_id = insert_working_spec_revision(
                    &mut tx,
                    &request,
                    &note_id,
                    "updated",
                    Some(previous_content.as_str()),
                    Some(content.as_str()),
                )
                .await?;

                PersistedWorkingSpec {
                    note_id,
                    permalink: permalink.clone(),
                    status: final_status.to_owned(),
                    created: false,
                    changed: true,
                    locked_task_status: locked_status,
                    task_terminal,
                    revision_id: Some(revision_id),
                }
            }
            None => {
                let content = render(None);
                let note_id = uuid::Uuid::now_v7().to_string();
                let scope_paths_json: serde_json::Value = serde_json::from_str(request.scope_paths)
                    .map_err(|e| {
                        Error::InvalidData(format!("invalid json for notes.scope_paths: {e}"))
                    })?;
                sqlx::query(
                    "INSERT INTO notes (id, project_id, permalink, title, file_path, storage, \
                     note_type, folder, status, tags, content, retrieval_anchor, content_hash, \
                     scope_paths, confidence) \
                     VALUES ($1, $2, $3, $4, '', 'db', 'design', $5, $6, $7, $8, NULL, $9, $10, 0.5)",
                )
                .bind(&note_id)
                .bind(request.project_id)
                .bind(&permalink)
                .bind(&title)
                .bind(folder_for_type("design"))
                .bind(final_status)
                .bind(working_spec_tag_json())
                .bind(&content)
                .bind(note_content_hash(&content))
                .bind(scope_paths_json)
                .execute(&mut *tx)
                .await?;
                index_links_for_note(&mut tx, &note_id, request.project_id, &content).await?;
                resolve_links_for_note(&mut tx, &note_id, &title, &permalink, request.project_id)
                    .await?;

                let revision_id = insert_working_spec_revision(
                    &mut tx,
                    &request,
                    &note_id,
                    "created",
                    None,
                    Some(content.as_str()),
                )
                .await?;

                PersistedWorkingSpec {
                    note_id,
                    permalink: permalink.clone(),
                    status: final_status.to_owned(),
                    created: true,
                    changed: true,
                    locked_task_status: locked_status,
                    task_terminal,
                    revision_id: Some(revision_id),
                }
            }
        };

        tx.commit().await?;
        Ok(outcome)
    }

    /// Promote one working-spec section into a new durable note.
    ///
    /// This is an explicit **copy**. The source working spec is never
    /// reactivated, retyped, edited, or re-tagged — a caller that wants the
    /// working spec changed must change it separately. The new note carries the
    /// working spec's own trusted session/task provenance on its creation
    /// revision and a `derived_from` link back to the source.
    ///
    /// Consolidation provenance is deliberately **not** seeded: promotion is an
    /// explicit editorial copy, not an extraction write, so the promoted note
    /// does not silently become a consolidation source for the originating
    /// session.
    pub async fn promote_working_spec_section(
        &self,
        request: PromoteWorkingSpecSection<'_>,
    ) -> Result<PromotedWorkingSpecNote> {
        self.db.ensure_initialized().await?;
        if !is_consolidation_eligible_note_type(request.note_type) {
            return Err(Error::InvalidData(format!(
                "promotion target must be case, pattern, or pitfall, got {}",
                request.note_type
            )));
        }
        if request.title.trim().is_empty() || request.content.trim().is_empty() {
            return Err(Error::InvalidData(
                "promoted note requires a nonblank title and content".to_owned(),
            ));
        }

        let mut tx = self.db.pool().begin().await?;

        let source = sqlx::query(
            "SELECT id, project_id, note_type FROM notes WHERE id = $1 AND project_id = $2",
        )
        .bind(request.working_spec_note_id)
        .bind(request.project_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(source) = source else {
            tx.rollback().await?;
            return Err(Error::InvalidData(format!(
                "working spec not found in project: {}",
                request.working_spec_note_id
            )));
        };
        let source_note_type: String = source.try_get("note_type")?;
        if source_note_type != "design" {
            tx.rollback().await?;
            return Err(Error::InvalidData(format!(
                "promotion source must be a design working spec, got {source_note_type}"
            )));
        }

        // Provenance is copied from the working spec's own trusted revision, so
        // the promoted note remains attributable to the session and task that
        // produced the context.
        let provenance = sqlx::query(
            "SELECT session_id, task_id, task_run_id FROM note_revision_events \
             WHERE note_id = $1 AND actor_kind = 'system' AND subsystem = $2 \
             ORDER BY note_seq DESC, created_at DESC, id DESC LIMIT 1",
        )
        .bind(request.working_spec_note_id)
        .bind(NoteRevisionSubsystem::Extraction.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let (session_id, task_id, task_run_id) = match provenance {
            Some(row) => (
                row.try_get::<Option<String>, _>("session_id")?,
                row.try_get::<Option<String>, _>("task_id")?,
                row.try_get::<Option<String>, _>("task_run_id")?,
            ),
            None => (None, None, None),
        };

        let note_id = uuid::Uuid::now_v7().to_string();
        let permalink = permalink_for(request.note_type, request.title);
        let scope_paths_json: serde_json::Value = serde_json::from_str(request.scope_paths)
            .map_err(|e| Error::InvalidData(format!("invalid json for notes.scope_paths: {e}")))?;
        let confidence = request
            .confidence
            .clamp(CONFIDENCE_FLOOR, CONFIDENCE_CEILING);

        sqlx::query(
            "INSERT INTO notes (id, project_id, permalink, title, file_path, storage, note_type, \
             folder, status, tags, content, retrieval_anchor, content_hash, scope_paths, confidence) \
             VALUES ($1, $2, $3, $4, '', 'db', $5, $6, 'active', '[]'::jsonb, $7, NULL, $8, $9, $10)",
        )
        .bind(&note_id)
        .bind(request.project_id)
        .bind(&permalink)
        .bind(request.title)
        .bind(request.note_type)
        .bind(folder_for_type(request.note_type))
        .bind(request.content)
        .bind(note_content_hash(request.content))
        .bind(scope_paths_json)
        .bind(confidence)
        .execute(&mut *tx)
        .await?;
        index_links_for_note(&mut tx, &note_id, request.project_id, request.content).await?;
        resolve_links_for_note(
            &mut tx,
            &note_id,
            request.title,
            &permalink,
            request.project_id,
        )
        .await?;

        let creation_revision_id = uuid::Uuid::now_v7().to_string();
        sqlx::query(
            "INSERT INTO note_revision_events (id, project_id, note_id, note_seq, event_kind, \
             content_before, content_after, confidence_before, confidence_after, actor_kind, \
             actor_id, subsystem, session_id, task_id, task_run_id, reason) \
             VALUES ($1, $2, $3, 1, 'created', NULL, $4, NULL, $5, 'system', NULL, $6, $7, $8, $9, $10)",
        )
        .bind(&creation_revision_id)
        .bind(request.project_id)
        .bind(&note_id)
        .bind(request.content)
        .bind(confidence)
        .bind(NoteRevisionSubsystem::Extraction.as_str())
        .bind(session_id.as_deref())
        .bind(task_id.as_deref())
        .bind(task_run_id.as_deref())
        .bind(request.reason.as_str())
        .execute(&mut *tx)
        .await?;

        // `derived_from` from the promoted note back to its working spec.
        let (a_id, b_id) = if note_id.as_str() <= request.working_spec_note_id {
            (note_id.clone(), request.working_spec_note_id.to_owned())
        } else {
            (request.working_spec_note_id.to_owned(), note_id.clone())
        };
        sqlx::query(
            "DELETE FROM note_associations WHERE note_a_id = $1 AND note_b_id = $2 \
             AND kind = 'co_access' AND source = 'session_co_access'",
        )
        .bind(&a_id)
        .bind(&b_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"INSERT INTO note_associations
                 (note_a_id, note_b_id, weight, co_access_count, last_co_access, kind, source)
               VALUES ($1, $2, 1.0, 0, to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
                       'derived_from', 'session_co_access')
               ON CONFLICT (note_a_id, note_b_id, kind, source) DO UPDATE SET
                   weight = GREATEST(note_associations.weight, EXCLUDED.weight),
                   kind = EXCLUDED.kind,
                   last_co_access = EXCLUDED.last_co_access"#,
        )
        .bind(&a_id)
        .bind(&b_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(PromotedWorkingSpecNote {
            note_id,
            permalink,
            creation_revision_id,
            session_id,
            task_id,
            task_run_id,
            derived_from_note_id: request.working_spec_note_id.to_owned(),
        })
    }
}

fn working_spec_tag_json() -> serde_json::Value {
    serde_json::Value::Array(vec![serde_json::Value::String(WORKING_SPEC_TAG.to_owned())])
}

async fn insert_working_spec_revision(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &PersistWorkingSpecRequest<'_>,
    note_id: &str,
    event_kind: &str,
    content_before: Option<&str>,
    content_after: Option<&str>,
) -> Result<String> {
    let seq: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(note_seq), 0) + 1 FROM note_revision_events \
         WHERE project_id = $1 AND note_id = $2",
    )
    .bind(request.project_id)
    .bind(note_id)
    .fetch_one(&mut **tx)
    .await?;
    let revision_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO note_revision_events (id, project_id, note_id, note_seq, event_kind, \
         content_before, content_after, confidence_before, confidence_after, actor_kind, \
         actor_id, subsystem, session_id, task_id, task_run_id, reason) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 0.5, 'system', NULL, $9, $10, $11, $12, $13)",
    )
    .bind(&revision_id)
    .bind(request.project_id)
    .bind(note_id)
    .bind(seq)
    .bind(event_kind)
    .bind(content_before)
    .bind(content_after)
    .bind(if event_kind == "created" {
        None
    } else {
        Some(0.5f64)
    })
    .bind(NoteRevisionSubsystem::Extraction.as_str())
    .bind(request.session_id)
    .bind(request.task_id)
    .bind(request.task_run_id)
    .bind(request.reason.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(revision_id)
}

/// Archive every active working spec belonging to `task_id`, inside the caller's
/// transaction.
///
/// Called by the terminal task transition while it holds the task row lock, so
/// the archive and the terminal state commit together. Idempotent: the
/// `status = 'active'` guard makes a repeated terminal delivery a no-op, and a
/// task with no working spec archives nothing.
///
/// Matching is by machine marking only — the reserved tag plus a trusted
/// extraction revision naming this task — so a hand-authored `design` note can
/// never be archived by a task transition.
pub(crate) async fn archive_task_working_specs_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task_id: &str,
    reason: &str,
) -> Result<Vec<String>> {
    let archived: Vec<(String, String)> = sqlx::query_as(
        r#"UPDATE notes n
           SET status = 'archived',
               lifecycle_changed_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'),
               updated_at = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
           WHERE n.status = 'active'
             AND n.note_type = 'design'
             AND n.storage = 'db'
             AND n.tags @> $1::jsonb
             AND EXISTS (
                   SELECT 1 FROM note_revision_events r
                   WHERE r.note_id = n.id
                     AND r.actor_kind = 'system'
                     AND r.subsystem = 'extraction'
                     AND r.task_id = $2
                 )
           RETURNING n.id, n.content"#,
    )
    .bind(working_spec_tag_json())
    .bind(task_id)
    .fetch_all(&mut **tx)
    .await?;

    for (note_id, content) in &archived {
        let project_id: String = sqlx::query_scalar("SELECT project_id FROM notes WHERE id = $1")
            .bind(note_id)
            .fetch_one(&mut **tx)
            .await?;
        let seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(note_seq), 0) + 1 FROM note_revision_events \
             WHERE project_id = $1 AND note_id = $2",
        )
        .bind(&project_id)
        .bind(note_id)
        .fetch_one(&mut **tx)
        .await?;
        sqlx::query(
            "INSERT INTO note_revision_events (id, project_id, note_id, note_seq, event_kind, \
             content_before, content_after, confidence_before, confidence_after, actor_kind, \
             actor_id, subsystem, session_id, task_id, task_run_id, reason) \
             VALUES ($1, $2, $3, $4, 'updated', $5, $5, 0.5, 0.5, 'system', NULL, 'extraction', \
             NULL, $6, NULL, $7)",
        )
        .bind(uuid::Uuid::now_v7().to_string())
        .bind(&project_id)
        .bind(note_id)
        .bind(seq)
        .bind(content)
        .bind(task_id)
        .bind(reason)
        .execute(&mut **tx)
        .await?;
    }

    Ok(archived.into_iter().map(|(id, _)| id).collect())
}
