//! Focused live MCP dispatch contract for reasoned memory revisions.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::auth_context::{REVISION_CALLER_CONTEXT, TrustedRevisionCallerContext};
use djinn_core::events::EventBus;
use djinn_db::{
    Database, NoteRepository, NoteRevisionCreateState, NoteRevisionDesiredState,
    NoteRevisionEventKind, NoteRevisionMutation, NoteRevisionReason, ProjectRepository,
    TrustedNoteRevisionAttribution, TrustedNoteRevisionProvenance,
};
use serde_json::{Value, json};

async fn harness_and_project() -> (McpTestHarness, String, String) {
    let db = Database::ephemeral()
        .await
        .expect("PostgreSQL test database");
    let harness = McpTestHarness::from_db(db.clone());
    let project = ProjectRepository::new(db, EventBus::noop())
        .create("revision-contract", "test", "revision-contract")
        .await
        .expect("project");
    (harness, project.slug(), project.id)
}

async fn revision_rows(
    db: &Database,
    project_id: &str,
) -> Vec<(
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
)> {
    sqlx::query_as(
        "SELECT event_kind, reason, actor_kind, actor_id, session_id, task_id, COALESCE(task_run_id, '') FROM note_revision_events WHERE project_id = $1 ORDER BY created_at, id",
    )
    .bind(project_id)
    .fetch_all(db.pool())
    .await
    .expect("revision rows")
}

async fn persisted_counts(db: &Database, project_id: &str) -> (i64, i64) {
    let notes = sqlx::query_scalar("SELECT COUNT(*) FROM notes WHERE project_id = $1")
        .bind(project_id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let revisions =
        sqlx::query_scalar("SELECT COUNT(*) FROM note_revision_events WHERE project_id = $1")
            .bind(project_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    (notes, revisions)
}

fn write(project: &str, title: &str, content: &str, note_type: &str, reason: Value) -> Value {
    json!({"project": project, "title": title, "content": content, "type": note_type, "reason": reason})
}

#[tokio::test]
async fn dispatch_rejects_invalid_or_spoofed_reason_inputs_before_durable_mutation() {
    let (harness, project, project_id) = harness_and_project().await;
    let caller = TrustedRevisionCallerContext::authenticated_human("human-contract");

    let expected_envelope = "invalid parameters: field: reason, message: reason must be non-blank";
    for reason in [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!(" \t\n")),
        Some(json!("\u{2003}\u{2002}")),
    ] {
        let mut args = write(&project, "Rejected", "initial", "reference", json!("valid"));
        let object = args.as_object_mut().unwrap();
        match reason {
            Some(value) => {
                object.insert("reason".into(), value);
            }
            None => {
                object.remove("reason");
            }
        }
        let error = REVISION_CALLER_CONTEXT
            .scope(caller.clone(), harness.call_tool("memory_write", args))
            .await
            .expect_err("invalid reason must reject")
            .to_string();
        assert!(
            error.contains(expected_envelope),
            "expected reason rejection envelope `{expected_envelope}` in error: {error}"
        );
        assert_eq!(persisted_counts(harness.db(), &project_id).await, (0, 0));
    }

    for key in [
        "actor_id",
        "actor_kind",
        "subsystem",
        "session_id",
        "task_id",
        "task_run_id",
    ] {
        let mut args = write(&project, "Spoofed", "initial", "reference", json!("valid"));
        args.as_object_mut()
            .unwrap()
            .insert(key.into(), json!("spoofed"));
        let error = REVISION_CALLER_CONTEXT
            .scope(caller.clone(), harness.call_tool("memory_write", args))
            .await
            .expect_err("spoofed input must reject")
            .to_string();
        assert!(error.contains(key), "{error}");
        assert_eq!(persisted_counts(harness.db(), &project_id).await, (0, 0));
    }
}

#[tokio::test]
async fn dispatch_persists_trimmed_reason_event_kind_and_trusted_provenance() {
    let (harness, project, project_id) = harness_and_project().await;
    let caller = TrustedRevisionCallerContext::authenticated_agent("agent-contract")
        .unwrap()
        .with_execution_provenance(
            Some("session-contract".into()),
            Some("task-contract".into()),
            Some("run-contract".into()),
        );
    let created = REVISION_CALLER_CONTEXT
        .scope(
            Some(caller),
            harness.call_tool(
                "memory_write",
                write(
                    &project,
                    "Revision Contract",
                    "initial",
                    "brief",
                    json!("  create brief  "),
                ),
            ),
        )
        .await
        .expect("create");
    REVISION_CALLER_CONTEXT
        .scope(
            TrustedRevisionCallerContext::authenticated_human("human-contract"),
            harness.call_tool(
                "memory_write",
                write(
                    &project,
                    "ignored singleton title",
                    "singleton content",
                    "brief",
                    json!(" singleton update "),
                ),
            ),
        )
        .await
        .expect("singleton update");
    let edited = REVISION_CALLER_CONTEXT.scope(TrustedRevisionCallerContext::authenticated_human("human-contract"), harness.call_tool("memory_edit", json!({"project": project, "identifier": created["permalink"], "operation": "append", "content": "more", "reason": " edit brief "}))).await.expect("edit");
    assert!(edited["content"].as_str().unwrap().contains("more"));
    REVISION_CALLER_CONTEXT.scope(TrustedRevisionCallerContext::authenticated_human("human-contract"), harness.call_tool("memory_delete", json!({"project": project, "identifier": created["permalink"], "reason": " delete brief "}))).await.expect("delete");

    let rows = revision_rows(harness.db(), &project_id).await;
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows[0],
        (
            "created".into(),
            "create brief".into(),
            "agent".into(),
            Some("agent-contract".into()),
            Some("session-contract".into()),
            Some("task-contract".into()),
            "run-contract".into()
        )
    );
    assert_eq!(
        rows[1],
        (
            "updated".into(),
            "singleton update".into(),
            "human".into(),
            Some("human-contract".into()),
            None,
            None,
            "".into()
        )
    );
    assert_eq!(
        rows[2],
        (
            "updated".into(),
            "edit brief".into(),
            "human".into(),
            Some("human-contract".into()),
            None,
            None,
            "".into()
        )
    );
    assert_eq!(
        rows[3],
        (
            "deleted".into(),
            "delete brief".into(),
            "human".into(),
            Some("human-contract".into()),
            None,
            None,
            "".into()
        )
    );
}

#[tokio::test]
async fn repository_failure_rolls_back_note_and_revision_together() {
    let (harness, _project, project_id) = harness_and_project().await;
    let repo = NoteRepository::new(harness.db().clone(), EventBus::noop());
    let note_id = uuid::Uuid::now_v7().to_string();
    repo.set_revision_event_insertion_failure_for_test(true);
    let result = repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project_id.clone(),
            note_id: Some(note_id.clone()),
            event_kind: NoteRevisionEventKind::Created,
            desired: NoteRevisionDesiredState::Create(NoteRevisionCreateState {
                title: "rollback".into(),
                permalink: "reference/rollback".into(),
                content: "must not persist".into(),
                note_type: "reference".into(),
                folder: "reference".into(),
                status: "active".into(),
                tags: "[]".into(),
                retrieval_anchor: None,
                scope_paths: "[]".into(),
                confidence: 0.5,
            }),
            attribution: TrustedNoteRevisionAttribution::human("human-contract").unwrap(),
            provenance: TrustedNoteRevisionProvenance::default(),
            reason: NoteRevisionReason::new("prove rollback").unwrap(),
        })
        .await;
    repo.set_revision_event_insertion_failure_for_test(false);
    assert!(result.is_err());
    assert!(repo.get(&note_id).await.unwrap().is_none());
    assert_eq!(persisted_counts(harness.db(), &project_id).await, (0, 0));
}

#[tokio::test]
async fn singleton_noop_emits_no_revision_or_retrieval_side_effect() {
    let (harness, project, project_id) = harness_and_project().await;
    let caller = TrustedRevisionCallerContext::authenticated_human("human-contract");
    REVISION_CALLER_CONTEXT
        .scope(
            caller.clone(),
            harness.call_tool(
                "memory_write",
                write(
                    &project,
                    "Brief",
                    "same",
                    "brief",
                    json!("create singleton"),
                ),
            ),
        )
        .await
        .expect("create singleton");
    let metrics = harness.state().retrieval_metrics();
    let before = metrics
        .snapshot()
        .unwrap()
        .aggregate(
            djinn_telemetry::memory_retrieval::RetrievalEntryPoint::JitPitfalls,
            djinn_telemetry::memory_retrieval::RetrievalOutcome::Empty,
        )
        .count;
    REVISION_CALLER_CONTEXT
        .scope(
            caller,
            harness.call_tool(
                "memory_write",
                write(
                    &project,
                    "ignored",
                    "same",
                    "brief",
                    json!("unchanged singleton"),
                ),
            ),
        )
        .await
        .expect("no-op singleton response");
    assert_eq!(persisted_counts(harness.db(), &project_id).await, (1, 1));
    assert_eq!(
        metrics
            .snapshot()
            .unwrap()
            .aggregate(
                djinn_telemetry::memory_retrieval::RetrievalEntryPoint::JitPitfalls,
                djinn_telemetry::memory_retrieval::RetrievalOutcome::Empty
            )
            .count,
        before,
        "unchanged singleton must not emit a retrieval observation"
    );
}
