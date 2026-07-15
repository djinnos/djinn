//! Contract tests for `session_*` + `task_timeline` MCP tools.
//!
//! Migrated from `server/src/mcp_contract_tests/session_tools.rs`.  The two
//! `*_returns_error_without_pool` tests remain in the server crate because
//! the stub `SlotPoolOps` we ship in the harness returns `Some(..)` with
//! query methods returning empties — it therefore does NOT surface the
//! "slot pool actor not initialized" error the tests assert on.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::events::EventBus;
use djinn_core::extension_diagnostics::{
    ExtensionLoadPhase, ExtensionLoadRemedyCode, ExtensionLoadSeverity, ExtensionLoadSourceKind,
};
use djinn_db::{
    ExtensionLoadDiagnosticRepository, InsertExtensionLoadDiagnostic, SessionMessageRepository,
    TaskRepository,
};
use serde_json::json;

#[tokio::test]
async fn session_list_returns_empty_for_task_without_sessions() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;
    let epic = common::create_test_epic(harness.db(), &project.id).await;
    let task = common::create_test_task(harness.db(), &project.id, &epic.id).await;

    let payload = harness
        .call_tool(
            "session_list",
            json!({ "task_id": task.id, "project": project.slug() }),
        )
        .await
        .expect("session_list should dispatch");
    assert_eq!(payload.get("error"), None);
    assert_eq!(
        payload.get("task_id").and_then(|v| v.as_str()),
        Some(task.id.as_str())
    );
    assert!(
        payload
            .get("sessions")
            .and_then(|v| v.as_array())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn task_timeline_returns_no_diagnostic_events_for_empty_task() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;
    let epic = common::create_test_epic(harness.db(), &project.id).await;
    let task = common::create_test_task(harness.db(), &project.id, &epic.id).await;

    let payload = harness
        .call_tool(
            "task_timeline",
            json!({ "task_id": task.id, "project": project.slug() }),
        )
        .await
        .expect("task_timeline should dispatch");

    assert_eq!(payload.get("error"), None);
    assert_eq!(
        payload.get("extension_load_diagnostic_events"),
        Some(&json!([])),
        "successful timelines without diagnostics must expose no diagnostic events"
    );
}

#[tokio::test]
async fn session_list_filters_by_project_and_task() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project_a = common::create_test_project(db).await;
    let epic_a = common::create_test_epic(db, &project_a.id).await;
    let task_a1 = common::create_test_task(db, &project_a.id, &epic_a.id).await;
    let task_a2 = common::create_test_task(db, &project_a.id, &epic_a.id).await;
    let project_b = common::create_test_project(db).await;
    let epic_b = common::create_test_epic(db, &project_b.id).await;
    let task_b1 = common::create_test_task(db, &project_b.id, &epic_b.id).await;
    let _s_a1_1 = common::create_test_session(db, &project_a.id, &task_a1.id).await;
    let _s_a1_2 = common::create_test_session(db, &project_a.id, &task_a1.id).await;
    let _s_a2 = common::create_test_session(db, &project_a.id, &task_a2.id).await;
    let _s_b1 = common::create_test_session(db, &project_b.id, &task_b1.id).await;

    let payload = harness
        .call_tool(
            "session_list",
            json!({ "task_id": task_a1.id, "project": project_a.slug() }),
        )
        .await
        .expect("session_list should dispatch");
    assert_eq!(payload.get("error"), None);
    let sessions = payload.get("sessions").and_then(|v| v.as_array()).unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(
        sessions
            .iter()
            .all(|s| s.get("task_id").and_then(|v| v.as_str()) == Some(task_a1.id.as_str()))
    );
    assert!(
        sessions
            .iter()
            .all(|s| s.get("project_id").and_then(|v| v.as_str()) == Some(project_a.id.as_str()))
    );
}

#[tokio::test]
async fn timeline_and_list_resolve_task_across_projects_regardless_of_project_arg() {
    // The Kanban board spans many projects: a task's sessions must resolve by
    // the task's (globally unique) id even when the caller passes a *different*
    // project — or none at all. Regression for the cross-project "task not
    // found" error in the session pane (the UI's selected project may differ
    // from the task's own project).
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project_a = common::create_test_project(db).await;
    let epic_a = common::create_test_epic(db, &project_a.id).await;
    let task_a = common::create_test_task(db, &project_a.id, &epic_a.id).await;
    let session_a = common::create_test_session(db, &project_a.id, &task_a.id).await;
    let owner_diagnostic = ExtensionLoadDiagnosticRepository::new(db.clone())
        .insert_or_increment(extension_diagnostic_input(
            &project_a.id,
            Some(&task_a.id),
            Some(&session_a.id),
            "owner-project-search",
        ))
        .await
        .expect("insert owning-project diagnostic");
    // A second, unrelated project the UI might have "selected".
    let project_b = common::create_test_project(db).await;

    // task_timeline with the WRONG project hint → resolves by global UUID.
    let payload = harness
        .call_tool(
            "task_timeline",
            json!({ "task_id": task_a.id, "project": project_b.slug() }),
        )
        .await
        .expect("task_timeline should dispatch");
    assert_eq!(
        payload.get("error"),
        None,
        "wrong-project hint must not 404: {payload:?}"
    );
    assert_eq!(
        payload
            .get("sessions")
            .and_then(|v| v.as_array())
            .map(|a| a.len()),
        Some(1)
    );
    assert_eq!(
        payload.get("extension_load_diagnostic_events"),
        Some(&json!([{
            "kind": "extension_load_diagnostic",
            "session_id": session_a.id.clone(),
            "timestamp": owner_diagnostic.last_seen_at.clone(),
            "diagnostic": serde_json::to_value(&owner_diagnostic)
                .expect("serialize canonical owner row"),
        }])),
        "wrong project hint must still read diagnostics from the task owner"
    );

    // task_timeline with NO project → still resolves by global UUID.
    let payload = harness
        .call_tool("task_timeline", json!({ "task_id": task_a.id }))
        .await
        .expect("task_timeline should dispatch");
    assert_eq!(
        payload.get("error"),
        None,
        "missing project must not 404: {payload:?}"
    );

    // session_list likewise resolves cross-project with no project arg.
    let payload = harness
        .call_tool("session_list", json!({ "task_id": task_a.id }))
        .await
        .expect("session_list should dispatch");
    assert_eq!(
        payload.get("error"),
        None,
        "session_list missing project must not 404: {payload:?}"
    );
    assert_eq!(
        payload.get("task_id").and_then(|v| v.as_str()),
        Some(task_a.id.as_str())
    );
}

#[tokio::test]
async fn session_show_returns_full_shape_with_tokens() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;
    let session = common::create_test_session(db, &project.id, &task.id).await;

    let payload = harness
        .call_tool(
            "session_show",
            json!({ "id": session.id, "project": project.slug() }),
        )
        .await
        .expect("session_show should dispatch");
    assert_eq!(payload.get("error"), None);
    for key in [
        "id",
        "task_id",
        "model_id",
        "agent_type",
        "status",
        "tokens_in",
        "tokens_out",
        "parked_reason",
    ] {
        assert!(payload.get(key).is_some(), "missing key {key}");
    }
    assert_eq!(
        payload.get("extension_load_diagnostics"),
        Some(&json!([])),
        "successful session_show must expose an empty diagnostics array"
    );
}

fn extension_diagnostic_input(
    project_id: &str,
    task_id: Option<&str>,
    session_id: Option<&str>,
    source_key: &str,
) -> InsertExtensionLoadDiagnostic {
    InsertExtensionLoadDiagnostic {
        project_id: project_id.to_owned(),
        task_id: task_id.map(str::to_owned),
        session_id: session_id.map(str::to_owned),
        load_attempt_id: uuid::Uuid::now_v7().to_string(),
        source_kind: ExtensionLoadSourceKind::ProjectMcp,
        source_key: source_key.to_owned(),
        phase: ExtensionLoadPhase::ToolsList,
        severity: ExtensionLoadSeverity::Error,
        summary: "tools/list returned invalid JSON".to_owned(),
        summary_fingerprint: format!("{source_key:0<64}"),
        remedy_code: ExtensionLoadRemedyCode::CheckServer,
        remedy: "Check the MCP server health and restart it.".to_owned(),
        first_seen_at: "2026-07-14T12:00:00.000Z".to_owned(),
        last_seen_at: "2026-07-14T12:00:00.000Z".to_owned(),
        created_at: "2026-07-14T12:00:00.000Z".to_owned(),
    }
}

#[tokio::test]
async fn session_show_exposes_canonical_session_diagnostics_not_doctor_rows() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;
    let session = common::create_test_session(db, &project.id, &task.id).await;
    let diagnostics = ExtensionLoadDiagnosticRepository::new(db.clone());

    let persisted = diagnostics
        .insert_or_increment(extension_diagnostic_input(
            &project.id,
            Some(&task.id),
            Some(&session.id),
            "project-search",
        ))
        .await
        .expect("insert session-associated diagnostic");
    diagnostics
        .insert_or_increment(extension_diagnostic_input(
            &project.id,
            None,
            None,
            "doctor-search",
        ))
        .await
        .expect("insert doctor-only diagnostic");

    let payload = harness
        .call_tool(
            "session_show",
            json!({ "id": session.id, "project": project.slug() }),
        )
        .await
        .expect("session_show should dispatch");

    assert_eq!(payload.get("error"), None);
    assert_eq!(
        payload.get("extension_load_diagnostics"),
        Some(&json!([
            serde_json::to_value(persisted).expect("serialize canonical row")
        ])),
        "session_show must serialize the canonical repository row unchanged"
    );
}

#[tokio::test]
async fn task_timeline_projects_canonical_session_diagnostics_once_per_identity() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;
    let session_one = common::create_test_session(db, &project.id, &task.id).await;
    let session_two = common::create_test_session(db, &project.id, &task.id).await;
    let unrelated_task = common::create_test_task(db, &project.id, &epic.id).await;
    let unrelated_session = common::create_test_session(db, &project.id, &unrelated_task.id).await;
    let diagnostics = ExtensionLoadDiagnosticRepository::new(db.clone());

    let retry_input = extension_diagnostic_input(
        &project.id,
        Some(&task.id),
        Some(&session_one.id),
        "retrying-search",
    );
    let first = diagnostics
        .insert_or_increment(retry_input.clone())
        .await
        .expect("insert first diagnostic observation");
    let retried = diagnostics
        .insert_or_increment(retry_input)
        .await
        .expect("increment diagnostic occurrence");
    assert_eq!(first.diagnostic_id, retried.diagnostic_id);
    assert_eq!(retried.occurrence_count, 2);
    let distinct = diagnostics
        .insert_or_increment(extension_diagnostic_input(
            &project.id,
            Some(&task.id),
            Some(&session_two.id),
            "other-search",
        ))
        .await
        .expect("insert distinct diagnostic identity");
    diagnostics
        .insert_or_increment(extension_diagnostic_input(
            &project.id,
            Some(&task.id),
            Some(&unrelated_session.id),
            "unrelated-session",
        ))
        .await
        .expect("insert unrelated-session diagnostic");
    diagnostics
        .insert_or_increment(extension_diagnostic_input(
            &project.id,
            None,
            None,
            "doctor-only",
        ))
        .await
        .expect("insert doctor-only diagnostic");

    let payload = harness
        .call_tool(
            "task_timeline",
            json!({ "task_id": task.id, "project": project.slug() }),
        )
        .await
        .expect("task_timeline should dispatch");

    assert_eq!(payload.get("error"), None);
    let events = payload
        .get("extension_load_diagnostic_events")
        .and_then(|value| value.as_array())
        .expect("successful timeline must expose diagnostic event array");
    assert_eq!(
        events.len(),
        2,
        "only returned-session identities belong on the timeline"
    );
    assert!(events.iter().all(|event| {
        event.get("kind").and_then(|value| value.as_str()) == Some("extension_load_diagnostic")
    }));
    for diagnostic in [&retried, &distinct] {
        let event = events
            .iter()
            .find(|event| {
                event
                    .get("diagnostic")
                    .and_then(|value| value.get("diagnostic_id"))
                    .and_then(|value| value.as_str())
                    == Some(diagnostic.diagnostic_id.as_str())
            })
            .expect("each persisted diagnostic identity must have one event");
        assert_eq!(
            event.get("diagnostic"),
            Some(&serde_json::to_value(diagnostic).expect("serialize canonical row")),
            "timeline event must preserve the canonical V1 payload"
        );
        assert_eq!(
            event.get("session_id").and_then(|value| value.as_str()),
            diagnostic.session_id.as_deref()
        );
        assert_eq!(
            event.get("timestamp").and_then(|value| value.as_str()),
            Some(diagnostic.last_seen_at.as_str())
        );
    }
}

#[tokio::test]
async fn session_show_wrong_project_cannot_expose_session_diagnostics() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let owning_project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &owning_project.id).await;
    let task = common::create_test_task(db, &owning_project.id, &epic.id).await;
    let session = common::create_test_session(db, &owning_project.id, &task.id).await;
    ExtensionLoadDiagnosticRepository::new(db.clone())
        .insert_or_increment(extension_diagnostic_input(
            &owning_project.id,
            Some(&task.id),
            Some(&session.id),
            "private-search",
        ))
        .await
        .expect("insert owning-project diagnostic");
    let other_project = common::create_test_project(db).await;

    let payload = harness
        .call_tool(
            "session_show",
            json!({ "id": session.id, "project": other_project.slug() }),
        )
        .await
        .expect("session_show should dispatch");

    assert!(
        payload
            .get("error")
            .and_then(|value| value.as_str())
            .is_some()
    );
    assert_eq!(payload.get("id"), None);
    assert_eq!(payload.get("extension_load_diagnostics"), None);
}

#[tokio::test]
async fn session_list_and_show_surface_parked_reason() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;
    let session = common::create_test_session(db, &project.id, &task.id).await;

    let repo = djinn_db::SessionRepository::new(db.clone(), EventBus::noop());
    repo.update(
        &session.id,
        djinn_core::models::SessionStatus::Completed,
        11,
        22,
        0,
        0,
        Some("budget".to_string()),
    )
    .await
    .expect("set parked_reason");

    let show = harness
        .call_tool(
            "session_show",
            json!({ "id": session.id, "project": project.slug() }),
        )
        .await
        .expect("session_show should dispatch");
    assert_eq!(
        show.get("parked_reason").and_then(|v| v.as_str()),
        Some("budget")
    );

    let list = harness
        .call_tool(
            "session_list",
            json!({ "task_id": task.id, "project": project.slug() }),
        )
        .await
        .expect("session_list should dispatch");
    let sessions = list.get("sessions").and_then(|v| v.as_array()).unwrap();
    assert_eq!(
        sessions[0].get("parked_reason").and_then(|v| v.as_str()),
        Some("budget")
    );
}

#[tokio::test]
async fn session_show_not_found_returns_error_shape() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;

    let payload = harness
        .call_tool(
            "session_show",
            json!({ "id": "missing-session-id", "project": project.slug() }),
        )
        .await
        .expect("session_show should dispatch");
    assert!(payload.get("error").and_then(|v| v.as_str()).is_some());
    assert_eq!(payload.get("id"), None);
}

#[tokio::test]
async fn task_timeline_returns_chronological_session_and_message_history() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;
    let s1 = common::create_test_session(db, &project.id, &task.id).await;
    let s2 = common::create_test_session(db, &project.id, &task.id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
    msg_repo
        .insert_message(
            &s1.id,
            &task.id,
            "user",
            &json!([{"type":"text","text":"first"}]).to_string(),
            None,
        )
        .await
        .unwrap();
    msg_repo
        .insert_message(
            &s2.id,
            &task.id,
            "assistant",
            &json!([{"type":"text","text":"second"}]).to_string(),
            None,
        )
        .await
        .unwrap();

    let payload = harness
        .call_tool(
            "task_timeline",
            json!({ "task_id": task.id, "project": project.slug() }),
        )
        .await
        .expect("task_timeline should dispatch");
    assert_eq!(payload.get("error"), None);
    let sessions = payload.get("sessions").and_then(|v| v.as_array()).unwrap();
    let messages = payload.get("messages").and_then(|v| v.as_array()).unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(messages.len(), 2);
    assert_eq!(
        payload.get("extension_load_diagnostic_events"),
        Some(&json!([])),
        "successful timeline without diagnostics must expose an empty event array"
    );
    let ts0 = messages[0]
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap();
    let ts1 = messages[1]
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(ts0 <= ts1);
}

#[tokio::test]
async fn task_timeline_renders_loop_guard_activity_distinctly() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    task_repo
        .log_activity(
            Some(&task.id),
            "agent-supervisor",
            "system",
            "loop_guard_tripped",
            &json!({
                "kind": "loop_guard_tripped",
                "details": {
                    "kind": "identical_tool_failure",
                    "offending_signature": "shell:cargo-test",
                    "threshold": 3,
                    "observed": 4,
                    "turn_span": { "start": 7, "end": 12 },
                    "session_id": "session-123"
                }
            })
            .to_string(),
        )
        .await
        .unwrap();
    task_repo
        .log_activity(
            Some(&task.id),
            "agent-supervisor",
            "system",
            "failed",
            &json!({ "kind": "failed", "details": { "reason": "provider fault" } }).to_string(),
        )
        .await
        .unwrap();
    task_repo
        .log_activity(
            Some(&task.id),
            "agent-supervisor",
            "system",
            "escalated",
            &json!({ "kind": "escalated", "details": { "reason": "needs input" } }).to_string(),
        )
        .await
        .unwrap();

    let payload = harness
        .call_tool(
            "task_timeline",
            json!({ "task_id": task.id, "project": project.slug() }),
        )
        .await
        .expect("task_timeline should dispatch");
    assert_eq!(payload.get("error"), None);
    let activity = payload.get("activity").and_then(|v| v.as_array()).unwrap();
    let loop_guard = activity
        .iter()
        .find(|entry| entry.get("kind").and_then(|v| v.as_str()) == Some("loop_guard_tripped"))
        .expect("timeline should include distinct loop_guard_tripped entry");
    assert_eq!(
        loop_guard
            .get("details")
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str()),
        Some("identical_tool_failure")
    );
    assert_eq!(
        loop_guard
            .get("details")
            .and_then(|v| v.get("offending_signature"))
            .and_then(|v| v.as_str()),
        Some("shell:cargo-test")
    );
    assert!(
        loop_guard
            .get("summary")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("turns 7..=12") && s.contains("shell:cargo-test"))
    );

    let activity_payload = harness
        .call_tool(
            "task_activity_list",
            json!({ "id": task.id, "project": project.slug() }),
        )
        .await
        .expect("task_activity_list should dispatch");
    assert_eq!(activity_payload.get("error"), None);
    let activity_entries = activity_payload
        .get("entries")
        .and_then(|value| value.as_array())
        .unwrap();
    let loop_guard_activity = activity_entries
        .iter()
        .find(|entry| {
            entry.get("kind").and_then(|value| value.as_str()) == Some("loop_guard_tripped")
        })
        .expect("activity feed should include distinct loop_guard_tripped entry");
    assert_eq!(
        loop_guard_activity
            .get("details")
            .and_then(|value| value.get("kind"))
            .and_then(|value| value.as_str()),
        Some("identical_tool_failure")
    );
    assert!(
        loop_guard_activity
            .get("summary")
            .and_then(|value| value.as_str())
            .is_some_and(
                |summary| summary.contains("turns 7..=12") && summary.contains("shell:cargo-test")
            )
    );
    assert!(
        activity
            .iter()
            .any(|entry| entry.get("kind").and_then(|v| v.as_str()) == Some("failed"))
    );
    assert!(
        activity
            .iter()
            .any(|entry| entry.get("kind").and_then(|v| v.as_str()) == Some("escalated"))
    );
}

#[tokio::test]
async fn task_timeline_not_found_returns_error_shape() {
    let harness = McpTestHarness::new().await;
    let project = common::create_test_project(harness.db()).await;

    let payload = harness
        .call_tool(
            "task_timeline",
            json!({ "task_id": "missing-task", "project": project.slug() }),
        )
        .await
        .expect("task_timeline should dispatch");
    assert!(payload.get("error").and_then(|v| v.as_str()).is_some());
    assert!(payload.get("sessions").is_none());
    assert!(payload.get("messages").is_none());
}

#[tokio::test]
async fn session_messages_returns_messages_for_valid_session_id() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let project = common::create_test_project(db).await;
    let epic = common::create_test_epic(db, &project.id).await;
    let task = common::create_test_task(db, &project.id, &epic.id).await;
    let sess = common::create_test_session(db, &project.id, &task.id).await;

    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
    msg_repo
        .insert_message(
            &sess.id,
            &task.id,
            "user",
            &json!([{"type":"text","text":"hello"}]).to_string(),
            None,
        )
        .await
        .unwrap();

    let payload = harness
        .call_tool(
            "session_messages",
            json!({ "id": sess.id, "project": project.slug() }),
        )
        .await
        .expect("session_messages should dispatch");
    assert_eq!(payload.get("error"), None);
    let messages = payload.get("messages").and_then(|v| v.as_array()).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].get("role").and_then(|v| v.as_str()),
        Some("user")
    );
}
