//! Working-spec lifecycle and promotion (proposal `t5rn`, T4 / AC8 + AC9).
//!
//! Every assertion reads a side effect — an actual note status, an actual tag,
//! an actual revision row, an actual edge, or an actual search hit — never a
//! return flag on its own.

use djinn_core::models::TransitionAction;

use super::*;
use crate::repositories::note::working_spec::{
    PersistWorkingSpecRequest, PromoteWorkingSpecSection, WORKING_SPEC_CONSTRAINT_SENTENCE,
    WORKING_SPEC_TAG, working_spec_permalink,
};

// ── fixture helpers ──────────────────────────────────────────────────────────

struct TaskFixture {
    id: String,
    short_id: String,
}

async fn make_task(db: &Database, project_id: &str, title: &str) -> TaskFixture {
    let epic_id = make_epic(db, project_id).await;
    let task = TaskRepository::new(db.clone(), EventBus::noop())
        .create_fixture_with_ac(
            &epic_id,
            title,
            "working spec fixture task",
            "working spec fixture design",
            "task",
            1,
            "worker",
            None,
            Some(r#"[{"title":"fixture-ac"}]"#),
        )
        .await
        .unwrap();
    TaskFixture {
        id: task.id,
        short_id: task.short_id,
    }
}

/// Drive the task to `closed` through the real transition path, so the archive
/// hook runs where it actually lives.
async fn close_task(db: &Database, task_id: &str) {
    TaskRepository::new(db.clone(), EventBus::noop())
        .transition(task_id, TransitionAction::Close, "w", "worker", None, None)
        .await
        .unwrap();
}

async fn reopen_task(db: &Database, task_id: &str) {
    TaskRepository::new(db.clone(), EventBus::noop())
        .transition(
            task_id,
            TransitionAction::Reopen,
            "w",
            "worker",
            Some("reopened for follow-up"),
            None,
        )
        .await
        .unwrap();
}

async fn task_status(db: &Database, task_id: &str) -> String {
    sqlx::query_scalar::<_, String>("SELECT status FROM tasks WHERE id = $1")
        .bind(task_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

fn working_spec_document(task_short_id: &str, body: &str) -> String {
    // Includes the exact generated constraint sentence the legacy migration
    // matches on, so fixtures and production prose cannot drift.
    format!(
        "# Working Spec\n\n## Active objective\n- Task {task_short_id}\n\n## Constraints\n- {WORKING_SPEC_CONSTRAINT_SENTENCE}\n\n## Captured session knowledge\n{body}"
    )
}

async fn persist(
    repo: &NoteRepository,
    project_id: &str,
    task: &TaskFixture,
    session_id: &str,
    body: &'static str,
) -> crate::error::DbResult<crate::repositories::note::working_spec::PersistedWorkingSpec> {
    let short_id = task.short_id.clone();
    let render = move |existing: Option<&str>| match existing {
        Some(existing) => format!("{existing}\n\n{body}"),
        None => working_spec_document(&short_id, body),
    };
    repo.persist_task_working_spec(
        PersistWorkingSpecRequest {
            project_id,
            task_id: &task.id,
            task_short_id: &task.short_id,
            session_id,
            task_run_id: Some("fixture-run"),
            scope_paths: "[]",
            reason: NoteRevisionReason::new("persisted extraction working specification").unwrap(),
        },
        &render,
    )
    .await
}

async fn note_row(
    db: &Database,
    project_id: &str,
    permalink: &str,
) -> Option<(String, String, String, String)> {
    sqlx::query_as::<_, (String, String, String, String)>(
        "SELECT id, status, tags::text, content FROM notes \
         WHERE project_id = $1 AND permalink = $2",
    )
    .bind(project_id)
    .bind(permalink)
    .fetch_optional(db.pool())
    .await
    .unwrap()
}

// ═══════════════════════════════════════════════════════════════════════════
// AC8 — machine marking, provenance, and the derived status
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn working_spec_is_machine_marked_with_tag_and_trusted_provenance() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let task = make_task(&db, &project.id, "Working spec marking").await;
    let session = make_session(&db, &project.id, Some(&task.id), "worker/spec").await;

    let persisted = persist(&repo, &project.id, &task, &session, "first body")
        .await
        .unwrap();
    assert!(persisted.created);
    assert!(persisted.changed);
    assert_eq!(persisted.status, "active");
    assert!(!persisted.task_terminal);

    let permalink = working_spec_permalink(&task.short_id);
    assert_eq!(persisted.permalink, permalink);
    let (note_id, status, tags, content) = note_row(&db, &project.id, &permalink).await.unwrap();
    assert_eq!(status, "active");
    assert!(tags.contains(WORKING_SPEC_TAG), "tags were {tags}");
    assert!(content.contains(WORKING_SPEC_CONSTRAINT_SENTENCE));

    // It stays a `design` note — no new note status, no retype.
    let note_type: String =
        sqlx::query_scalar::<_, String>("SELECT note_type FROM notes WHERE id = $1")
            .bind(&note_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(note_type, "design");

    // Trusted revision provenance carries task, task-run, session, and the
    // extraction subsystem.
    let (subsystem, actor_kind, rev_session, rev_task, rev_task_run): (
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT subsystem, actor_kind, session_id, task_id, task_run_id \
         FROM note_revision_events WHERE note_id = $1 ORDER BY note_seq DESC LIMIT 1",
    )
    .bind(&note_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(subsystem.as_deref(), Some("extraction"));
    assert_eq!(actor_kind, "system");
    assert_eq!(rev_session.as_deref(), Some(session.as_str()));
    assert_eq!(rev_task.as_deref(), Some(task.id.as_str()));
    assert_eq!(rev_task_run.as_deref(), Some("fixture-run"));
}

/// Interleaving 1: the task reaches terminal state BEFORE the working spec's
/// first insert. The note must never land active.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_before_first_insert_never_creates_an_active_working_spec() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let task = make_task(&db, &project.id, "Terminal before insert").await;
    let session = make_session(&db, &project.id, Some(&task.id), "worker/spec").await;

    close_task(&db, &task.id).await;
    assert_eq!(task_status(&db, &task.id).await, "closed");

    let persisted = persist(&repo, &project.id, &task, &session, "late body")
        .await
        .unwrap();
    assert!(persisted.created, "content is still persisted");
    assert!(persisted.task_terminal);
    assert_eq!(
        persisted.status, "archived",
        "a terminal task must not receive an active working spec"
    );

    let permalink = working_spec_permalink(&task.short_id);
    let (_, status, _, content) = note_row(&db, &project.id, &permalink).await.unwrap();
    assert_eq!(status, "archived");
    assert!(
        content.contains("late body"),
        "the content is preserved even though the note is archived"
    );
    assert_eq!(active_working_spec_count(&db, &task.id).await, 0);
}

/// Interleaving 2: a reopened task is being persisted to when a fresh terminal
/// transition lands. Whichever order the lock grants, no active working spec may
/// survive for a terminal task.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_racing_reactivation_leaves_no_active_working_spec() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let task = make_task(&db, &project.id, "Terminal racing reactivation").await;
    let session = make_session(&db, &project.id, Some(&task.id), "worker/spec").await;

    // An active working spec exists, then the task is closed and reopened, so a
    // later persistence would reactivate it.
    persist(&repo, &project.id, &task, &session, "body one")
        .await
        .unwrap();
    close_task(&db, &task.id).await;
    reopen_task(&db, &task.id).await;
    assert_eq!(task_status(&db, &task.id).await, "open");

    // Race: reactivating persistence vs a fresh terminal transition.
    let persist_db = db.clone();
    let persist_project = project.id.clone();
    let persist_task = TaskFixture {
        id: task.id.clone(),
        short_id: task.short_id.clone(),
    };
    let persist_session = session.clone();
    let persist_tx = tx.clone();
    let persister = tokio::spawn(async move {
        let repo = NoteRepository::new(persist_db, event_bus_for(&persist_tx));
        persist(
            &repo,
            &persist_project,
            &persist_task,
            &persist_session,
            "racing body",
        )
        .await
    });
    let close_db = db.clone();
    let close_task_id = task.id.clone();
    let closer = tokio::spawn(async move {
        close_task(&close_db, &close_task_id).await;
    });

    let persisted = persister.await.unwrap().unwrap();
    closer.await.unwrap();

    assert_eq!(task_status(&db, &task.id).await, "closed");
    // Whichever won, the invariant is the same: nothing active remains.
    assert_eq!(
        active_working_spec_count(&db, &task.id).await,
        0,
        "persist reported status {} (task_terminal={})",
        persisted.status,
        persisted.task_terminal
    );
    let permalink = working_spec_permalink(&task.short_id);
    let (_, status, _, _) = note_row(&db, &project.id, &permalink).await.unwrap();
    assert_eq!(status, "archived");
}

async fn active_working_spec_count(db: &Database, task_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"SELECT COUNT(*) FROM notes n
           WHERE n.status = 'active'
             AND n.note_type = 'design'
             AND n.tags @> '["working-spec"]'::jsonb
             AND EXISTS (
                   SELECT 1 FROM note_revision_events r
                   WHERE r.note_id = n.id AND r.actor_kind = 'system'
                     AND r.subsystem = 'extraction' AND r.task_id = $1
                 )"#,
    )
    .bind(task_id)
    .fetch_one(db.pool())
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_then_persistence_reactivates_the_same_permalink() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let task = make_task(&db, &project.id, "Reopen reactivates").await;
    let session = make_session(&db, &project.id, Some(&task.id), "worker/spec").await;

    let first = persist(&repo, &project.id, &task, &session, "body one")
        .await
        .unwrap();
    let permalink = working_spec_permalink(&task.short_id);

    close_task(&db, &task.id).await;
    let (_, archived_status, _, _) = note_row(&db, &project.id, &permalink).await.unwrap();
    assert_eq!(
        archived_status, "archived",
        "terminal transition archived it"
    );

    // Reopen alone must NOT eagerly reactivate.
    reopen_task(&db, &task.id).await;
    let (_, after_reopen, _, _) = note_row(&db, &project.id, &permalink).await.unwrap();
    assert_eq!(
        after_reopen, "archived",
        "reopen must not eagerly reactivate the note"
    );

    // The next successful persistence reactivates that SAME permalink.
    let second = persist(&repo, &project.id, &task, &session, "body two")
        .await
        .unwrap();
    assert!(!second.created, "the same note is reused, not a new one");
    assert_eq!(second.note_id, first.note_id);
    assert_eq!(second.permalink, permalink);
    assert_eq!(second.status, "active");

    let (_, status, _, content) = note_row(&db, &project.id, &permalink).await.unwrap();
    assert_eq!(status, "active");
    assert!(content.contains("body one") && content.contains("body two"));

    // Exactly one note carries this permalink — reactivation, not duplication.
    let count: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM notes WHERE project_id = $1 AND permalink = $2",
    )
    .bind(&project.id)
    .bind(&permalink)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistence_and_terminal_transition_are_both_idempotent() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let task = make_task(&db, &project.id, "Idempotent lifecycle").await;
    let session = make_session(&db, &project.id, Some(&task.id), "worker/spec").await;

    persist(&repo, &project.id, &task, &session, "stable body")
        .await
        .unwrap();
    let permalink = working_spec_permalink(&task.short_id);

    // Re-persisting identical content is a no-op: no new revision row.
    let note_id = note_row(&db, &project.id, &permalink).await.unwrap().0;
    let revisions_before = revision_count_for(&db, &note_id).await;
    let repeat = repo
        .persist_task_working_spec(
            PersistWorkingSpecRequest {
                project_id: &project.id,
                task_id: &task.id,
                task_short_id: &task.short_id,
                session_id: &session,
                task_run_id: Some("fixture-run"),
                scope_paths: "[]",
                reason: NoteRevisionReason::new("persisted extraction working specification")
                    .unwrap(),
            },
            &|existing: Option<&str>| existing.unwrap_or_default().to_owned(),
        )
        .await
        .unwrap();
    assert!(!repeat.changed, "identical content must be a no-op");
    assert!(repeat.revision_id.is_none());
    assert_eq!(revision_count_for(&db, &note_id).await, revisions_before);

    // Closing twice archives once and then does nothing.
    close_task(&db, &task.id).await;
    let after_first_close = revision_count_for(&db, &note_id).await;
    let (_, status, _, _) = note_row(&db, &project.id, &permalink).await.unwrap();
    assert_eq!(status, "archived");

    // A second terminal delivery from an already-closed task is rejected by the
    // state machine; drive the idempotent archive directly instead by reopening
    // and closing again with nothing active to archive.
    reopen_task(&db, &task.id).await;
    close_task(&db, &task.id).await;
    assert_eq!(
        revision_count_for(&db, &note_id).await,
        after_first_close,
        "a terminal transition with no ACTIVE working spec must archive nothing"
    );
    assert_eq!(active_working_spec_count(&db, &task.id).await, 0);
}

async fn revision_count_for(db: &Database, note_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM note_revision_events WHERE note_id = $1")
        .bind(note_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_authored_design_notes_are_never_touched_by_task_termination() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let task = make_task(&db, &project.id, "Human design untouched").await;
    let session = make_session(&db, &project.id, Some(&task.id), "worker/spec").await;

    // A hand-authored design note that deliberately mimics a working spec in
    // every way EXCEPT the machine marking: same note type, same document shape,
    // and the verbatim constraint sentence the legacy predicate looks for. Only
    // its permalink differs, so it is a genuinely separate row.
    //
    // (A title that slugifies onto the *canonical* working-spec permalink is a
    // different situation entirely — see
    // `human_note_occupying_the_canonical_permalink_is_adopted_as_pre_existing_behaviour`.)
    let human = repo
        .create_db_note(
            &project.id,
            "Architecture working notes for the retry subsystem",
            &working_spec_document(&task.short_id, "hand written architecture notes"),
            "design",
            "[]",
        )
        .await
        .unwrap();
    assert_ne!(
        human.permalink,
        working_spec_permalink(&task.short_id),
        "the fixture must be a genuinely distinct row"
    );

    persist(&repo, &project.id, &task, &session, "machine body")
        .await
        .unwrap();
    close_task(&db, &task.id).await;

    let (_, human_status, human_tags, human_content) =
        note_row(&db, &project.id, &human.permalink).await.unwrap();
    assert_eq!(
        human_status, "active",
        "a design note without machine marking must stay active"
    );
    assert!(
        !human_tags.contains(WORKING_SPEC_TAG),
        "no tag may be added to a hand-authored note; tags were {human_tags}"
    );
    assert_eq!(human_content, human.content, "content must be untouched");
    assert_eq!(
        revision_count_for(&db, &human.id).await,
        0,
        "no revision may be written against a hand-authored note"
    );

    // The machine-marked one WAS archived, proving the fixture actually
    // exercised the terminal path.
    let permalink = working_spec_permalink(&task.short_id);
    let (_, machine_status, _, _) = note_row(&db, &project.id, &permalink).await.unwrap();
    assert_eq!(machine_status, "archived");
}

/// Records a real, **pre-existing** sharp edge rather than hiding it.
///
/// Working-spec persistence is keyed on the canonical permalink, so a
/// hand-authored `design` note that happens to slugify onto
/// `design/working-spec-{short_id}` is adopted and thereafter treated as the
/// task's working spec. Note that `slugify` lowercases, so a differently-cased
/// title collides too.
///
/// This is unchanged behaviour: the previous implementation also did
/// `get_by_permalink(...)` followed by an update, so the same adoption happened
/// before this proposal. It is asserted here so the boundary is documented and
/// any future change to it is a deliberate, visible decision — not so that it is
/// endorsed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn human_note_occupying_the_canonical_permalink_is_adopted_as_pre_existing_behaviour() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let task = make_task(&db, &project.id, "Canonical permalink collision").await;
    let session = make_session(&db, &project.id, Some(&task.id), "worker/spec").await;

    // Differently-cased title, identical slug.
    let colliding = repo
        .create_db_note(
            &project.id,
            &format!("Working Spec {}", task.short_id.to_uppercase()),
            "hand written content",
            "design",
            "[]",
        )
        .await
        .unwrap();
    assert_eq!(
        colliding.permalink,
        working_spec_permalink(&task.short_id),
        "the fixture must actually collide"
    );

    let persisted = persist(&repo, &project.id, &task, &session, "machine body")
        .await
        .unwrap();
    assert_eq!(
        persisted.note_id, colliding.id,
        "persistence adopts the row already occupying the canonical permalink"
    );
    assert!(!persisted.created);
}

// ═══════════════════════════════════════════════════════════════════════════
// AC9 — explicit promotion
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_copies_a_section_into_a_searchable_durable_note() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let task = make_task(&db, &project.id, "Promotion source").await;
    let session = make_session(&db, &project.id, Some(&task.id), "worker/promote").await;

    let persisted = persist(
        &repo,
        &project.id,
        &task,
        &session,
        "retry storms amplify duplicate recovery work",
    )
    .await
    .unwrap();
    let permalink = working_spec_permalink(&task.short_id);
    let (spec_id, spec_status, spec_tags, spec_content) =
        note_row(&db, &project.id, &permalink).await.unwrap();
    let spec_revisions = revision_count_for(&db, &spec_id).await;

    let promoted = repo
        .promote_working_spec_section(PromoteWorkingSpecSection {
            project_id: &project.id,
            working_spec_note_id: &persisted.note_id,
            note_type: "pattern",
            title: "Retry storms amplify duplicate recovery work",
            content: "## Context\nRetry storms amplify duplicate recovery work during incident \
                      recovery.\n\n## Guidance\nPrefer idempotent recovery steps with backoff.",
            scope_paths: "[]",
            confidence: 0.6,
            reason: NoteRevisionReason::new("promoted working spec section").unwrap(),
        })
        .await
        .unwrap();

    // A new ACTIVE durable note of the requested type.
    let (promoted_status, promoted_type): (String, String) =
        sqlx::query_as("SELECT status, note_type FROM notes WHERE id = $1")
            .bind(&promoted.note_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(promoted_status, "active");
    assert_eq!(promoted_type, "pattern");

    // Provenance preserved from the working spec's own trusted revision.
    assert_eq!(promoted.session_id.as_deref(), Some(session.as_str()));
    assert_eq!(promoted.task_id.as_deref(), Some(task.id.as_str()));
    assert_eq!(promoted.task_run_id.as_deref(), Some("fixture-run"));
    let (rev_session, rev_task, rev_subsystem): (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT session_id, task_id, subsystem FROM note_revision_events WHERE id = $1",
        )
        .bind(&promoted.creation_revision_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(rev_session.as_deref(), Some(session.as_str()));
    assert_eq!(rev_task.as_deref(), Some(task.id.as_str()));
    assert_eq!(rev_subsystem.as_deref(), Some("extraction"));

    // A `derived_from` link back to the working spec.
    let edge = repo
        .get_association_kind(&promoted.note_id, &persisted.note_id)
        .await
        .unwrap()
        .expect("derived_from edge recorded");
    assert_eq!(edge.1, "derived_from");

    // The SOURCE working spec is completely unchanged: status, tags, content,
    // type, and revision count.
    let (_, status_after, tags_after, content_after) =
        note_row(&db, &project.id, &permalink).await.unwrap();
    assert_eq!(status_after, spec_status, "source status must not change");
    assert_eq!(tags_after, spec_tags, "source tags must not change");
    assert_eq!(
        content_after, spec_content,
        "source content must not change"
    );
    assert_eq!(
        revision_count_for(&db, &spec_id).await,
        spec_revisions,
        "promotion must not append a revision to the source"
    );
    let spec_type: String =
        sqlx::query_scalar::<_, String>("SELECT note_type FROM notes WHERE id = $1")
            .bind(&spec_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(spec_type, "design", "promotion must not retype the source");

    // Retrievable through NORMAL durable-note search — actually searched for,
    // not read back by id.
    let hits = repo
        .search(NoteSearchParams {
            project_id: &project.id,
            query: "retry storms duplicate recovery",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 20,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: None,
        })
        .await
        .unwrap();
    assert!(
        hits.iter().any(|hit| hit.id == promoted.note_id),
        "the promoted note must be reachable through normal search; hits were {:?}",
        hits.iter().map(|hit| &hit.permalink).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_rejects_ineligible_targets_and_non_design_sources() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let task = make_task(&db, &project.id, "Promotion guards").await;
    let session = make_session(&db, &project.id, Some(&task.id), "worker/promote").await;
    let persisted = persist(&repo, &project.id, &task, &session, "body")
        .await
        .unwrap();

    fn request<'a>(
        project_id: &'a str,
        source: &'a str,
        note_type: &'a str,
    ) -> PromoteWorkingSpecSection<'a> {
        PromoteWorkingSpecSection {
            project_id,
            working_spec_note_id: source,
            note_type,
            title: "Guarded",
            content: "content",
            scope_paths: "[]",
            confidence: 0.5,
            reason: NoteRevisionReason::new("promoted working spec section").unwrap(),
        }
    }

    // A non-durable target type is refused.
    let error = repo
        .promote_working_spec_section(request(&project.id, &persisted.note_id, "design"))
        .await
        .expect_err("design is not a durable promotion target");
    assert!(error.to_string().contains("case, pattern, or pitfall"));

    // A non-design source is refused.
    let durable = repo
        .create_db_note(&project.id, "Not a spec", "body", "pattern", "[]")
        .await
        .unwrap();
    let error = repo
        .promote_working_spec_section(request(&project.id, &durable.id, "case"))
        .await
        .expect_err("only a design working spec may be promoted from");
    assert!(error.to_string().contains("design working spec"));

    // Neither rejection created anything.
    let promoted_count: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM notes WHERE project_id = $1 AND note_type = 'case'",
    )
    .bind(&project.id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(promoted_count, 0);
}
