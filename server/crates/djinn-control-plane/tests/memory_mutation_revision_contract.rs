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
use djinn_db::{NoteRepository, NoteRevisionEvent};
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

async fn assert_tenant_state_unchanged(
    repo: &NoteRepository,
    owner_project_id: &str,
    other_project_id: &str,
    owner_note_id: &str,
    other_note_id: &str,
    owner_note_before: &Value,
    other_note_before: &Value,
    owner_revisions_before: &[NoteRevisionEvent],
    other_revisions_before: &[NoteRevisionEvent],
) {
    let owner_note = repo
        .get(owner_note_id)
        .await
        .expect("reload owner note")
        .expect("owner note remains present");
    let other_note = repo
        .get(other_note_id)
        .await
        .expect("reload other note")
        .expect("other note remains present");
    assert_eq!(&owner_note.to_value(), owner_note_before);
    assert_eq!(&other_note.to_value(), other_note_before);
    assert_eq!(
        repo.revision_events(owner_project_id)
            .await
            .expect("reload owner revisions"),
        owner_revisions_before
    );
    assert_eq!(
        repo.revision_events(other_project_id)
            .await
            .expect("reload other revisions"),
        other_revisions_before
    );
}

#[tokio::test]
async fn cross_project_mutations_and_revision_history_do_not_disclose_foreign_notes() {
    let harness = McpTestHarness::new().await;
    let (owner_project, _owner_dir) = common::create_test_project_with_dir(harness.db()).await;
    let (other_project, _other_dir) = common::create_test_project_with_dir(harness.db()).await;
    let owner_project_ref = owner_project.slug();
    let other_project_ref = other_project.slug();
    let owner = TrustedRevisionCallerContext::authenticated_human("owner-user").unwrap();
    let other = TrustedRevisionCallerContext::authenticated_human("other-user").unwrap();

    let created = dispatch_as(
        owner.clone(),
        &harness,
        "memory_write",
        json!({
            "project": owner_project_ref,
            "title": "Owner-only revision fixture",
            "content": "owner content",
            "type": "reference",
            "reason": "create owner-only revision fixture"
        }),
    )
    .await;
    assert!(
        created.get("error").is_none() || created["error"].is_null(),
        "owner write failed: {created}"
    );
    let owner_note_id = created["id"].as_str().expect("owner note id").to_owned();
    let owner_permalink = created["permalink"]
        .as_str()
        .expect("owner permalink")
        .to_owned();

    // Candidate lookup is project-scoped: an exact-content collision must not
    // reuse or merge the owner note through the live write-dedup path.
    let other_dedup_write = dispatch_as(
        other.clone(),
        &harness,
        "memory_write",
        json!({
            "project": other_project_ref,
            "title": "Other tenant dedup-isolation fixture",
            "content": "owner content",
            "type": "reference",
            "reason": "attempt cross-project write dedup"
        }),
    )
    .await;
    assert!(other_dedup_write["error"].is_null());
    assert_ne!(other_dedup_write["id"], created["id"]);
    assert_eq!(other_dedup_write["deduplicated"], false);
    let other_note_id = other_dedup_write["id"]
        .as_str()
        .expect("other note id")
        .to_owned();
    let other_permalink = other_dedup_write["permalink"]
        .as_str()
        .expect("other permalink")
        .to_owned();
    assert_eq!(owner_permalink, "reference/owner-only-revision-fixture");
    assert_eq!(
        other_permalink,
        "reference/other-tenant-dedup-isolation-fixture"
    );
    assert_ne!(owner_permalink, other_permalink);

    // The normal repository query is project-scoped. It proves that the owner
    // has an immutable row while the other project cannot query it.
    let repo = revision_repo(&harness);
    let owner_revisions_before = repo
        .revision_events(&owner_project.id)
        .await
        .expect("owner revision query");
    assert_eq!(owner_revisions_before.len(), 1);
    assert_eq!(
        owner_revisions_before[0].note_id.as_deref(),
        Some(owner_note_id.as_str())
    );
    assert_eq!(owner_revisions_before[0].event_kind, "created");
    assert_eq!(
        owner_revisions_before[0].reason,
        "create owner-only revision fixture"
    );
    let other_revisions_before = repo
        .revision_events(&other_project.id)
        .await
        .expect("other project revision query");
    assert_eq!(other_revisions_before.len(), 1);
    assert_eq!(
        other_revisions_before[0].note_id.as_deref(),
        other_dedup_write["id"].as_str()
    );
    let owner_note_before = repo
        .get(&owner_note_id)
        .await
        .expect("capture owner note")
        .expect("owner note exists")
        .to_value();
    let other_note_before = repo
        .get(&other_note_id)
        .await
        .expect("capture other note")
        .expect("other note exists")
        .to_value();

    let absent_permalink = "reference/no-such-owner-note";
    let foreign_edit = dispatch_as(
        other.clone(),
        &harness,
        "memory_edit",
        json!({
            "project": other_project_ref,
            "identifier": owner_permalink,
            "operation": "append",
            "content": "foreign mutation",
            "reason": "attempt cross-project edit"
        }),
    )
    .await;
    assert_tenant_state_unchanged(
        &repo,
        &owner_project.id,
        &other_project.id,
        &owner_note_id,
        &other_note_id,
        &owner_note_before,
        &other_note_before,
        &owner_revisions_before,
        &other_revisions_before,
    )
    .await;
    let absent_edit = dispatch_as(
        other.clone(),
        &harness,
        "memory_edit",
        json!({
            "project": other_project_ref,
            "identifier": absent_permalink,
            "operation": "append",
            "content": "absent mutation",
            "reason": "attempt absent edit"
        }),
    )
    .await;
    assert_eq!(
        foreign_edit.as_object().unwrap().len(),
        absent_edit.as_object().unwrap().len()
    );
    assert_eq!(
        foreign_edit["error"],
        format!("note not found: {owner_permalink}")
    );
    assert_eq!(
        absent_edit["error"],
        format!("note not found: {absent_permalink}")
    );

    let foreign_delete = dispatch_as(
        other.clone(),
        &harness,
        "memory_delete",
        json!({
            "project": other_project_ref,
            "identifier": owner_permalink,
            "reason": "attempt cross-project delete"
        }),
    )
    .await;
    assert_tenant_state_unchanged(
        &repo,
        &owner_project.id,
        &other_project.id,
        &owner_note_id,
        &other_note_id,
        &owner_note_before,
        &other_note_before,
        &owner_revisions_before,
        &other_revisions_before,
    )
    .await;
    let absent_delete = dispatch_as(
        other.clone(),
        &harness,
        "memory_delete",
        json!({
            "project": other_project_ref,
            "identifier": absent_permalink,
            "reason": "attempt absent delete"
        }),
    )
    .await;
    assert_eq!(
        foreign_delete.as_object().unwrap().len(),
        absent_delete.as_object().unwrap().len()
    );
    assert_eq!(foreign_delete["ok"], false);
    assert_eq!(
        foreign_delete["error"],
        format!("note not found: {owner_permalink}")
    );
    assert_eq!(
        absent_delete["error"],
        format!("note not found: {absent_permalink}")
    );

    // Cross-tenant immutable revision query/history through the production MCP
    // surface. The handler resolves the caller's project, so a foreign permalink
    // is indistinguishable from a genuinely absent one.
    let foreign_revisions = dispatch_as(
        other.clone(),
        &harness,
        "memory_revisions",
        json!({"project": other_project_ref, "permalink": owner_permalink}),
    )
    .await;
    assert_tenant_state_unchanged(
        &repo,
        &owner_project.id,
        &other_project.id,
        &owner_note_id,
        &other_note_id,
        &owner_note_before,
        &other_note_before,
        &owner_revisions_before,
        &other_revisions_before,
    )
    .await;
    let absent_revisions = dispatch_as(
        other,
        &harness,
        "memory_revisions",
        json!({"project": other_project_ref, "permalink": absent_permalink}),
    )
    .await;
    assert_eq!(
        foreign_revisions.as_object().unwrap().len(),
        absent_revisions.as_object().unwrap().len()
    );
    assert_eq!(foreign_revisions["revisions"], json!([]));
    assert_eq!(
        foreign_revisions["error"],
        format!("note not found: {owner_permalink}")
    );
    assert_eq!(
        absent_revisions["error"],
        format!("note not found: {absent_permalink}")
    );

    // The authorized tenant can query the exact owned immutable rows through the
    // same production MCP surface.
    let owner_revisions_dispatch = dispatch_as(
        owner,
        &harness,
        "memory_revisions",
        json!({"project": owner_project_ref, "permalink": owner_permalink}),
    )
    .await;
    assert!(
        owner_revisions_dispatch.get("error").is_none()
            || owner_revisions_dispatch["error"].is_null(),
        "authorized revision query failed: {owner_revisions_dispatch}"
    );
    let owner_dispatched = owner_revisions_dispatch["revisions"]
        .as_array()
        .expect("authorized revisions array");
    assert_eq!(owner_dispatched.len(), 1);
    assert_eq!(owner_dispatched[0]["note_id"], owner_note_id);
    assert_eq!(owner_dispatched[0]["event_kind"], "created");
    assert_eq!(
        owner_dispatched[0]["reason"],
        "create owner-only revision fixture"
    );

    // Tenant A's permalink never resolves to tenant B's distinct fixture.
    assert!(
        repo.get_by_permalink(&other_project.id, &owner_permalink)
            .await
            .expect("other note lookup")
            .is_none(),
        "foreign note must not appear in other project"
    );
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
    assert!(
        created.get("error").is_none() || created["error"].is_null(),
        "write error: {created}"
    );
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
    assert!(
        edited.get("error").is_none() || edited["error"].is_null(),
        "edit error: {edited}"
    );
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
async fn live_singleton_rewrite_uses_trusted_revision_context() {
    let harness = McpTestHarness::new().await;
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project_ref = project.slug();
    let created = dispatch_as(
        TrustedRevisionCallerContext::authenticated_human("singleton-human").unwrap(),
        &harness,
        "memory_write",
        json!({"project": project_ref, "title": "Ignored", "content": "singleton before", "type": "brief", "reason": "create singleton"}),
    )
    .await;
    assert!(
        created.get("error").is_none() || created["error"].is_null(),
        "singleton create error: {created}"
    );
    let note_id = created["id"].as_str().expect("singleton id").to_owned();

    let rewritten = dispatch_as(
        TrustedRevisionCallerContext::authenticated_agent("singleton-agent")
            .unwrap()
            .with_execution_provenance(
                Some("singleton-session".to_owned()),
                Some("singleton-task".to_owned()),
                Some("singleton-run".to_owned()),
            ),
        &harness,
        "memory_write",
        json!({"project": project_ref, "title": "Ignored", "content": "singleton after", "type": "brief", "reason": "\u{2003} rewrite singleton through live dispatch \u{2003}"}),
    )
    .await;
    assert!(
        rewritten.get("error").is_none() || rewritten["error"].is_null(),
        "singleton rewrite error: {rewritten}"
    );
    assert_eq!(rewritten["id"].as_str(), Some(note_id.as_str()));

    let repo = revision_repo(&harness);
    let revisions = repo
        .revision_events(&project.id)
        .await
        .expect("singleton revisions");
    assert_eq!(revisions.len(), 2);
    let revision = &revisions[1];
    assert_eq!(revision.note_id.as_deref(), Some(note_id.as_str()));
    assert_eq!(revision.event_kind, "updated");
    assert_eq!(revision.reason, "rewrite singleton through live dispatch");
    assert_eq!(revision.actor_kind, "agent");
    assert_eq!(revision.actor_id.as_deref(), Some("singleton-agent"));
    assert_eq!(revision.subsystem, None);
    assert_eq!(revision.content_before.as_deref(), Some("singleton before"));
    assert_eq!(revision.content_after.as_deref(), Some("singleton after"));
    assert_eq!(
        (
            revision.session_id.as_deref(),
            revision.task_id.as_deref(),
            revision.task_run_id.as_deref()
        ),
        (
            Some("singleton-session"),
            Some("singleton-task"),
            Some("singleton-run")
        ),
    );

    let untrusted = harness
        .call_tool(
            "memory_write",
            json!({"project": project_ref, "title": "Ignored", "content": "must not persist", "type": "brief", "reason": "untrusted singleton overwrite"}),
        )
        .await
        .expect("untrusted dispatch returns tool response");
    assert_eq!(untrusted["error"], "authenticated revision caller required");
    assert_eq!(
        repo.revision_events(&project.id)
            .await
            .expect("revisions after rejection")
            .len(),
        2
    );
    assert_eq!(
        repo.get(&note_id)
            .await
            .expect("singleton after rejection")
            .expect("singleton survives")
            .content,
        "singleton after",
    );
}

#[tokio::test]
async fn live_type_changing_empty_edit_is_atomic_with_its_revision() {
    let harness = McpTestHarness::new().await;
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project_ref = project.slug();
    let context = TrustedRevisionCallerContext::authenticated_agent("move-agent")
        .unwrap()
        .with_execution_provenance(
            Some("move-session".into()),
            Some("move-task".into()),
            Some("move-run".into()),
        );
    let created = dispatch_as(context.clone(), &harness, "memory_write", json!({"project": project_ref, "title": "Empty Move", "content": "", "type": "reference", "reason": "create empty move fixture"})).await;
    let note_id = created["id"].as_str().unwrap().to_owned();
    let moved = dispatch_as(context.clone(), &harness, "memory_edit", json!({"project": project_ref, "identifier": created["permalink"], "operation": "append", "content": "", "type": "design", "reason": "\u{2003} move empty note atomically \u{2003}"})).await;
    assert_eq!(moved["note_type"], "design");
    assert_eq!(moved["folder"], "design");
    assert_eq!(moved["permalink"], "design/empty-move");
    assert_eq!(moved["content"], "");
    let repo = revision_repo(&harness);
    let revisions = repo.revision_events(&project.id).await.unwrap();
    assert_eq!(revisions.len(), 2);
    let revision = &revisions[1];
    assert_eq!(revision.note_id.as_deref(), Some(note_id.as_str()));
    assert_eq!(revision.event_kind, "updated");
    assert_eq!(revision.content_before.as_deref(), Some(""));
    assert_eq!(revision.content_after.as_deref(), Some(""));
    assert_eq!(revision.reason, "move empty note atomically");
    assert_eq!(
        (
            revision.actor_kind.as_str(),
            revision.actor_id.as_deref(),
            revision.subsystem.as_deref()
        ),
        ("agent", Some("move-agent"), None)
    );
    assert_eq!(
        (
            revision.session_id.as_deref(),
            revision.task_id.as_deref(),
            revision.task_run_id.as_deref()
        ),
        (Some("move-session"), Some("move-task"), Some("move-run"))
    );

    let failed = dispatch_as(context, &harness, "memory_edit", json!({"project": project_ref, "identifier": "design/empty-move", "operation": "find_replace", "find_text": "missing", "content": "replacement", "type": "reference", "reason": "failed move must roll back"})).await;
    assert!(failed["error"].as_str().unwrap().contains("text not found"));
    let durable = repo.get(&note_id).await.unwrap().unwrap();
    assert_eq!(
        (
            durable.note_type.as_str(),
            durable.folder.as_str(),
            durable.permalink.as_str(),
            durable.content.as_str()
        ),
        ("design", "design", "design/empty-move", "")
    );
    assert_eq!(repo.revision_events(&project.id).await.unwrap().len(), 2);
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
