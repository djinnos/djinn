//! Live MCP mutation dispatch contract for trusted immutable revision attribution.
//!
//! These assertions deliberately use the production router and repository query
//! surface rather than manufacturing ledger rows in test setup.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::{
    auth_context::{REVISION_CALLER_CONTEXT, TrustedRevisionCallerContext},
    events::EventBus,
};
use djinn_db::NoteRepository;
use serde_json::{Value, json};

async fn dispatch_as(
    context: TrustedRevisionCallerContext,
    harness: &McpTestHarness,
    tool: &str,
    arguments: Value,
) -> Value {
    REVISION_CALLER_CONTEXT
        .scope(Some(context), harness.call_tool(tool, arguments))
        .await
        .unwrap_or_else(|error| panic!("{tool} dispatch failed: {error}"))
}

fn revision_repo(harness: &McpTestHarness) -> NoteRepository {
    NoteRepository::new(harness.db().clone(), EventBus::noop())
}

fn assert_invalid_parameters(result: anyhow::Result<Value>, label: &str) {
    let error = result.expect_err("{label} must be rejected before handler mutation");
    let error = error.to_string();
    assert!(
        error.contains("invalid parameters: field: reason, message: reason must be non-blank")
            || error.contains("unknown field"),
        "{label} must use the dispatch validation path: {error}"
    );
}

#[tokio::test]
async fn live_memory_mutations_persist_trusted_attribution_and_snapshots() {
    let harness = McpTestHarness::new().await;
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project_ref = project.slug();

    let created = dispatch_as(
        TrustedRevisionCallerContext::authenticated_human("human-live").unwrap(),
        &harness,
        "memory_write",
        json!({
            "project": project_ref,
            "title": "Live Revision Note",
            "content": "before",
            "type": "reference",
            "reason": "\u{2003} create through live dispatch \u{2003}"
        }),
    )
    .await;
    assert!(created.get("error").is_none(), "write error: {created}");
    let note_id = created["id"].as_str().expect("created note id").to_owned();

    let edited = dispatch_as(
        TrustedRevisionCallerContext::authenticated_agent("agent-live")
            .unwrap()
            .with_execution_provenance(
                Some("session-live".to_owned()),
                Some("task-live".to_owned()),
                Some("run-live".to_owned()),
            ),
        &harness,
        "memory_edit",
        json!({
            "project": project_ref,
            "identifier": created["permalink"],
            "operation": "append",
            "content": "after",
            "reason": "\u{2003} append through live dispatch \u{2003}"
        }),
    )
    .await;
    assert!(edited.get("error").is_none(), "edit error: {edited}");
    assert_eq!(edited["content"], "before\n\nafter");

    let deleted = dispatch_as(
        TrustedRevisionCallerContext::authenticated_human("human-delete").unwrap(),
        &harness,
        "memory_delete",
        json!({
            "project": project_ref,
            "identifier": created["permalink"],
            "reason": "\u{2003} delete through live dispatch \u{2003}"
        }),
    )
    .await;
    assert_eq!(deleted["ok"], true, "delete error: {deleted}");

    let revisions = revision_repo(&harness)
        .revision_events(&project.id)
        .await
        .expect("query immutable revisions through repository");
    assert_eq!(revisions.len(), 3);

    let created_revision = &revisions[0];
    assert_eq!(created_revision.note_id.as_deref(), Some(note_id.as_str()));
    assert_eq!(created_revision.event_kind, "created");
    assert_eq!(created_revision.reason, "create through live dispatch");
    assert_eq!(created_revision.actor_kind, "human");
    assert_eq!(created_revision.actor_id.as_deref(), Some("human-live"));
    assert_eq!(created_revision.subsystem, None);
    assert_eq!(created_revision.content_before, None);
    assert_eq!(created_revision.content_after.as_deref(), Some("before"));
    assert_eq!(
        (
            created_revision.session_id.as_deref(),
            created_revision.task_id.as_deref(),
            created_revision.task_run_id.as_deref(),
        ),
        (None, None, None),
        "authenticated callers outside an execution context retain null provenance"
    );

    let updated_revision = &revisions[1];
    assert_eq!(updated_revision.event_kind, "updated");
    assert_eq!(updated_revision.reason, "append through live dispatch");
    assert_eq!(updated_revision.actor_kind, "agent");
    assert_eq!(updated_revision.actor_id.as_deref(), Some("agent-live"));
    assert_eq!(updated_revision.subsystem, None);
    assert_eq!(updated_revision.content_before.as_deref(), Some("before"));
    assert_eq!(
        updated_revision.content_after.as_deref(),
        Some("before\n\nafter")
    );
    assert_eq!(
        (
            updated_revision.session_id.as_deref(),
            updated_revision.task_id.as_deref(),
            updated_revision.task_run_id.as_deref(),
        ),
        (Some("session-live"), Some("task-live"), Some("run-live")),
    );

    let deleted_revision = &revisions[2];
    assert_eq!(deleted_revision.event_kind, "deleted");
    assert_eq!(deleted_revision.reason, "delete through live dispatch");
    assert_eq!(deleted_revision.actor_kind, "human");
    assert_eq!(deleted_revision.actor_id.as_deref(), Some("human-delete"));
    assert_eq!(deleted_revision.subsystem, None);
    assert_eq!(
        deleted_revision.content_before.as_deref(),
        Some("before\n\nafter")
    );
    assert_eq!(deleted_revision.content_after, None);
    assert_eq!(
        (
            deleted_revision.session_id.as_deref(),
            deleted_revision.task_id.as_deref(),
            deleted_revision.task_run_id.as_deref(),
        ),
        (None, None, None),
    );
    assert!(
        revision_repo(&harness)
            .get(&note_id)
            .await
            .expect("query deleted note")
            .is_none(),
        "delete revision and note removal commit together"
    );
}

#[tokio::test]
async fn invalid_reasons_and_spoofed_attribution_do_not_mutate_notes_or_revisions() {
    let harness = McpTestHarness::new().await;
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project_ref = project.slug();
    let human = TrustedRevisionCallerContext::authenticated_human("human-validation").unwrap();

    let fixture = dispatch_as(
        human.clone(),
        &harness,
        "memory_write",
        json!({
            "project": project_ref,
            "title": "Validation Fixture",
            "content": "stable",
            "type": "reference",
            "reason": "create validation fixture"
        }),
    )
    .await;
    let fixture_id = fixture["id"].as_str().expect("fixture id").to_owned();
    let fixture_permalink = fixture["permalink"]
        .as_str()
        .expect("fixture permalink")
        .to_owned();
    let repo = revision_repo(&harness);
    let revision_count = repo
        .revision_events(&project.id)
        .await
        .expect("initial revisions")
        .len();

    let invalid_reasons = [
        None,
        Some(Value::Null),
        Some(json!("")),
        Some(json!("\u{2003}\t\n")),
    ];
    for (tool, mut arguments) in [
        (
            "memory_write",
            json!({"project": project_ref, "title": "Rejected Write", "content": "new", "type": "reference"}),
        ),
        (
            "memory_edit",
            json!({"project": project_ref, "identifier": fixture_permalink, "operation": "append", "content": "new"}),
        ),
        (
            "memory_delete",
            json!({"project": project_ref, "identifier": fixture_permalink}),
        ),
    ] {
        for reason in &invalid_reasons {
            if let Some(reason) = reason {
                arguments["reason"] = reason.clone();
            } else {
                arguments
                    .as_object_mut()
                    .expect("arguments object")
                    .remove("reason");
            }
            let result = REVISION_CALLER_CONTEXT
                .scope(
                    Some(human.clone()),
                    harness.call_tool(tool, arguments.clone()),
                )
                .await;
            assert_invalid_parameters(result, &format!("{tool} invalid reason"));
            assert_eq!(
                repo.revision_events(&project.id)
                    .await
                    .expect("revisions after rejection")
                    .len(),
                revision_count,
                "{tool} invalid reason must not append a revision"
            );
            assert_eq!(
                repo.get(&fixture_id)
                    .await
                    .expect("fixture after rejection")
                    .expect("fixture survives")
                    .content,
                "stable",
                "{tool} invalid reason must not alter note state"
            );
        }
    }

    for field in [
        "actor",
        "actor_id",
        "actor_kind",
        "subsystem",
        "session_id",
        "task_id",
        "task_run_id",
        "provenance",
    ] {
        let mut arguments = json!({
            "project": project_ref,
            "identifier": fixture_permalink,
            "operation": "append",
            "content": "spoofed",
            "reason": "attempt spoofed trusted field"
        });
        arguments[field] = json!("caller-controlled");
        let result = REVISION_CALLER_CONTEXT
            .scope(
                Some(human.clone()),
                harness.call_tool("memory_edit", arguments),
            )
            .await;
        assert_invalid_parameters(result, &format!("memory_edit spoofed {field}"));
        assert_eq!(
            repo.revision_events(&project.id)
                .await
                .expect("revisions after spoof rejection")
                .len(),
            revision_count,
            "spoofed {field} must not append a revision"
        );
        assert_eq!(
            repo.get(&fixture_id)
                .await
                .expect("fixture after spoof rejection")
                .expect("fixture survives")
                .content,
            "stable",
            "spoofed {field} must not alter note state"
        );
    }
}
