//! Focused live MCP dispatch contract for reasoned memory revisions.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::auth_context::{REVISION_CALLER_CONTEXT, TrustedRevisionCallerContext};
use djinn_core::events::EventBus;
use djinn_db::{Database, ProjectRepository};
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
    Option<String>,
    Option<String>,
    Option<String>,
    String,
)> {
    sqlx::query_as(
        "SELECT event_kind, reason, actor_id, session_id, task_id, COALESCE(task_run_id, '') FROM note_revision_events WHERE project_id = $1 ORDER BY created_at, id",
    )
    .bind(project_id)
    .fetch_all(db.pool())
    .await
    .expect("revision rows")
}

fn write(project: &str, title: &str, note_type: &str, reason: Value) -> Value {
    json!({"project": project, "title": title, "content": "initial", "type": note_type, "reason": reason})
}

#[tokio::test]
async fn dispatch_rejects_invalid_or_spoofed_reason_inputs_before_durable_mutation() {
    let (harness, project, project_id) = harness_and_project().await;
    let caller = TrustedRevisionCallerContext::authenticated_human("human-contract");

    for reason in [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!(" \t\n")),
        Some(json!("\u{2003}\u{2002}")),
    ] {
        let mut args = write(&project, "Rejected", "reference", json!("valid"));
        let object = args.as_object_mut().unwrap();
        match reason {
            Some(value) => {
                object.insert("reason".into(), value);
            }
            None => {
                object.remove("reason");
            }
        }
        let result = REVISION_CALLER_CONTEXT
            .scope(caller.clone(), harness.call_tool("memory_write", args))
            .await;
        let error = result.expect_err("invalid reason must reject").to_string();
        assert!(error.contains("reason"), "{error}");
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM note_revision_events WHERE project_id = $1"
            )
            .bind(&project_id)
            .fetch_one(harness.db().pool())
            .await
            .unwrap(),
            0
        );
    }

    for key in [
        "actor_id",
        "actor_kind",
        "subsystem",
        "session_id",
        "task_id",
        "task_run_id",
    ] {
        let mut args = write(&project, "Spoofed", "reference", json!("valid"));
        args.as_object_mut()
            .unwrap()
            .insert(key.into(), json!("spoofed"));
        let error = REVISION_CALLER_CONTEXT
            .scope(caller.clone(), harness.call_tool("memory_write", args))
            .await
            .expect_err("spoofed input must reject")
            .to_string();
        assert!(error.contains(key), "{error}");
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM note_revision_events WHERE project_id = $1"
        )
        .bind(&project_id)
        .fetch_one(harness.db().pool())
        .await
        .unwrap(),
        0
    );
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
            Some("human-contract".into()),
            None,
            None,
            "".into()
        )
    );
    assert_eq!(rows[2].0, "updated");
    assert_eq!(rows[2].1, "edit brief");
    assert_eq!(rows[3].0, "deleted");
    assert_eq!(rows[3].1, "delete brief");
}
