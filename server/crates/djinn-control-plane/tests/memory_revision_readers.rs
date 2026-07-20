//! Live MCP dispatch contract for the ledger-backed revision readers:
//! `memory_history`, `memory_diff`, and `memory_session_diff`.
//!
//! These assertions use the production dispatch path (`dispatch_tool`) and the
//! repository query surface for fixture setup/verification. No git behavior
//! remains: the restored tools read only the immutable ledger.

#[path = "common/mod.rs"]
mod common;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::auth_context::{
    REVISION_CALLER_CONTEXT, SESSION_USER_ID, TrustedRevisionCallerContext,
};
use djinn_core::events::EventBus;
use djinn_db::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};
use djinn_db::{
    NoteRepository, NoteRevisionDesiredState, NoteRevisionEventKind, NoteRevisionMutation,
    NoteRevisionReason, TrustedNoteRevisionAttribution, TrustedNoteRevisionProvenance,
    UserRepository,
};
use serde_json::{Value, json};

fn owner_context() -> TrustedRevisionCallerContext {
    TrustedRevisionCallerContext::authenticated_human("owner-user").unwrap()
}

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

async fn dispatch(harness: &McpTestHarness, tool: &str, arguments: Value) -> Value {
    harness
        .call_tool(tool, arguments)
        .await
        .unwrap_or_else(|error| panic!("{tool} dispatch failed: {error}"))
}

async fn dispatch_as_user(
    harness: &McpTestHarness,
    user_id: String,
    tool: &str,
    arguments: Value,
) -> Value {
    SESSION_USER_ID
        .scope(Some(user_id), harness.call_tool(tool, arguments))
        .await
        .unwrap_or_else(|error| panic!("{tool} dispatch failed: {error}"))
}

async fn seed_user(harness: &McpTestHarness, github_id: i64, login: &str, admin: bool) -> String {
    let user = UserRepository::new(harness.db().clone())
        .upsert_from_github(github_id, login, None, None)
        .await
        .expect("seed user");
    if admin {
        UserRepository::new(harness.db().clone())
            .set_admin_status(&user.id, true)
            .await
            .expect("promote user");
    }
    user.id
}

fn revision_repo(harness: &McpTestHarness) -> NoteRepository {
    NoteRepository::new(harness.db().clone(), EventBus::noop())
}

/// Write the canonical two-revision fixture: `created` (seq 1) then `updated`
/// (seq 2) through the real mutation tools. Returns
/// `(project, note_id, created_revision_id, updated_revision_id)`.
async fn seed_two_revision_note(
    harness: &McpTestHarness,
) -> (djinn_core::models::Project, String, String, String) {
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let created = dispatch_as(
        owner_context(),
        harness,
        "memory_write",
        json!({
            "project": project.slug(),
            "title": "Reader fixture note",
            "content": "alpha\nbeta\n",
            "type": "reference",
            "reason": "create reader fixture"
        }),
    )
    .await;
    assert!(created["error"].is_null(), "write failed: {created}");
    let note_id = created["id"].as_str().expect("note id").to_owned();

    let edited = dispatch_as(
        owner_context(),
        harness,
        "memory_edit",
        json!({
            "project": project.slug(),
            "identifier": note_id,
            "operation": "find_replace",
            "find_text": "beta",
            "content": "BETA",
            "reason": "edit reader fixture"
        }),
    )
    .await;
    assert!(edited["error"].is_null(), "edit failed: {edited}");

    let events = revision_repo(harness)
        .revision_events_for_note(&project.id, &note_id)
        .await
        .expect("load revisions");
    assert_eq!(events.len(), 2, "expected created+updated revisions");
    assert_eq!(events[0].event_kind, "created");
    assert_eq!(events[1].event_kind, "updated");
    (project, note_id, events[0].id.clone(), events[1].id.clone())
}

/// Append a confidence-only revision (seq N) through the transactional
/// boundary, optionally attributed to a session/task-run execution.
async fn seed_confidence_revision(
    harness: &McpTestHarness,
    project_id: &str,
    note_id: &str,
    provenance: Option<(&str, &str, &str)>,
) -> String {
    let note = revision_repo(harness)
        .get(note_id)
        .await
        .expect("load note")
        .expect("note exists");
    let provenance = match provenance {
        Some((session_id, task_id, task_run_id)) => TrustedNoteRevisionProvenance::new(
            Some(session_id.to_owned()),
            Some(task_id.to_owned()),
            Some(task_run_id.to_owned()),
        )
        .expect("provenance"),
        None => TrustedNoteRevisionProvenance::default(),
    };
    let result = revision_repo(harness)
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project_id.to_owned(),
            note_id: Some(note_id.to_owned()),
            event_kind: NoteRevisionEventKind::ConfidenceChanged,
            desired: NoteRevisionDesiredState::Existing {
                content: note.content.clone(),
                confidence: 0.9,
            },
            attribution: TrustedNoteRevisionAttribution::human("owner-user").expect("attribution"),
            provenance,
            reason: NoteRevisionReason::new("bump confidence").expect("reason"),
        })
        .await
        .expect("confidence mutation");
    result.revision_id.expect("confidence revision id")
}

fn assert_event_keys(event: &Value) {
    let keys: Vec<&str> = event
        .as_object()
        .expect("event object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "revision_id",
            "note_id",
            "note_seq",
            "event_kind",
            "content_before",
            "content_after",
            "confidence_before",
            "confidence_after",
            "actor_kind",
            "actor_id",
            "subsystem",
            "session_id",
            "task_id",
            "task_run_id",
            "reason",
            "created_at",
            "content_redacted",
        ],
        "revision event shape must stay exactly pinned"
    );
}

// ── memory_history ───────────────────────────────────────────────────────────

#[tokio::test]
async fn history_returns_newest_first_events_for_live_note() {
    let harness = McpTestHarness::new().await;
    let (project, note_id, created_revision, updated_revision) =
        seed_two_revision_note(&harness).await;

    let response = dispatch(
        &harness,
        "memory_history",
        json!({"project": project.slug(), "note_id": note_id}),
    )
    .await;
    assert!(response["error"].is_null(), "history failed: {response}");
    assert_eq!(response["history_start"], "ledger");
    assert!(response["next_cursor"].is_null());
    let keys: Vec<&str> = response
        .as_object()
        .expect("response object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec!["events", "next_cursor", "history_start", "error"],
        "history response carries only the approved fields (no git surface)"
    );

    let events = response["events"].as_array().expect("events array");
    assert_eq!(events.len(), 2);
    // Newest-first by note sequence: updated (seq 2) precedes created (seq 1).
    let (newest, oldest) = (&events[0], &events[1]);
    assert_event_keys(newest);
    assert_eq!(newest["revision_id"], updated_revision);
    assert_eq!(newest["event_kind"], "updated");
    assert_eq!(newest["note_seq"], 2);
    assert_eq!(newest["note_id"], note_id);
    assert_eq!(newest["content_before"], "alpha\nbeta\n");
    assert_eq!(newest["content_after"], "alpha\nBETA\n");
    assert_eq!(newest["reason"], "edit reader fixture");
    assert_eq!(newest["actor_kind"], "human");
    assert_eq!(newest["actor_id"], "owner-user");
    assert!(newest["subsystem"].is_null());
    assert!(newest["session_id"].is_null());
    assert!(newest["task_id"].is_null());
    assert!(newest["task_run_id"].is_null());
    assert_eq!(newest["content_redacted"], false);
    assert!(newest["created_at"].is_string());

    assert_eq!(oldest["revision_id"], created_revision);
    assert_eq!(oldest["event_kind"], "created");
    assert_eq!(oldest["note_seq"], 1);
    assert!(oldest["content_before"].is_null());
    assert_eq!(oldest["content_after"], "alpha\nbeta\n");
    assert_eq!(oldest["reason"], "create reader fixture");
    assert!(oldest["confidence_before"].is_null());
    assert_eq!(oldest["confidence_after"], 0.5);
}

#[tokio::test]
async fn history_paginates_with_stable_cursor_and_limit_bounds() {
    let harness = McpTestHarness::new().await;
    let (project, note_id, created_revision, updated_revision) =
        seed_two_revision_note(&harness).await;

    let first = dispatch(
        &harness,
        "memory_history",
        json!({"project": project.slug(), "note_id": note_id, "limit": 1}),
    )
    .await;
    assert!(first["error"].is_null(), "first page failed: {first}");
    assert_eq!(first["events"].as_array().unwrap().len(), 1);
    assert_eq!(first["events"][0]["revision_id"], updated_revision);
    assert_eq!(first["history_start"], "ledger");
    let cursor = first["next_cursor"]
        .as_str()
        .expect("second page cursor")
        .to_owned();

    let second = dispatch(
        &harness,
        "memory_history",
        json!({
            "project": project.slug(),
            "note_id": note_id,
            "limit": 1,
            "before_cursor": cursor,
        }),
    )
    .await;
    assert!(second["error"].is_null(), "second page failed: {second}");
    assert_eq!(second["events"].as_array().unwrap().len(), 1);
    assert_eq!(second["events"][0]["revision_id"], created_revision);
    assert!(second["next_cursor"].is_null(), "history exhausted");

    // The maximum public limit is accepted.
    let bounded = dispatch(
        &harness,
        "memory_history",
        json!({"project": project.slug(), "note_id": note_id, "limit": 200}),
    )
    .await;
    assert!(bounded["error"].is_null(), "max limit failed: {bounded}");
    assert_eq!(bounded["events"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn history_rejects_malformed_limits_and_cursors() {
    let harness = McpTestHarness::new().await;
    let (project, note_id, _, _) = seed_two_revision_note(&harness).await;

    for limit in [0, -1, 201] {
        let response = dispatch(
            &harness,
            "memory_history",
            json!({"project": project.slug(), "note_id": note_id, "limit": limit}),
        )
        .await;
        assert_eq!(
            response["error"],
            "invalid parameters: field: limit, message: limit must be between 1 and 200",
            "limit {limit} must be rejected at the public boundary"
        );
        assert!(response["events"].as_array().unwrap().is_empty());
        assert!(response["history_start"].is_null());
    }

    let response = dispatch(
        &harness,
        "memory_history",
        json!({"project": project.slug(), "note_id": note_id, "before_cursor": "not-a-cursor"}),
    )
    .await;
    assert_eq!(
        response["error"],
        "invalid parameters: field: before_cursor, message: invalid cursor"
    );

    // A session-view cursor must not replay against the note-history view.
    let (exec_project, _task, session, _run, exec_note_id) =
        seed_executed_revision_fixture(&harness).await;
    let session_page = dispatch(
        &harness,
        "memory_session_diff",
        json!({"project": exec_project.slug(), "session_id": session.id, "limit": 1}),
    )
    .await;
    assert!(
        session_page["error"].is_null(),
        "session page: {session_page}"
    );
    let session_cursor = session_page["next_cursor"]
        .as_str()
        .expect("session cursor")
        .to_owned();
    let replayed = dispatch(
        &harness,
        "memory_history",
        json!({
            "project": exec_project.slug(),
            "note_id": exec_note_id,
            "before_cursor": session_cursor,
        }),
    )
    .await;
    assert_eq!(
        replayed["error"], "invalid parameters: field: before_cursor, message: invalid cursor",
        "cross-view cursor replay must be rejected"
    );
}

#[tokio::test]
async fn history_treats_inaccessible_and_cross_project_notes_as_not_found() {
    let harness = McpTestHarness::new().await;
    let (owner_project, note_id, _, _) = seed_two_revision_note(&harness).await;
    let (other_project, _dir) = common::create_test_project_with_dir(harness.db()).await;

    let foreign = dispatch(
        &harness,
        "memory_history",
        json!({"project": other_project.slug(), "note_id": note_id}),
    )
    .await;
    let absent_id = uuid::Uuid::now_v7().to_string();
    let absent = dispatch(
        &harness,
        "memory_history",
        json!({"project": owner_project.slug(), "note_id": absent_id}),
    )
    .await;
    // The not-found envelope is identical apart from the echoed identifier,
    // so a cross-project note is indistinguishable from an unknown one.
    let mut foreign_shape = foreign.as_object().expect("foreign shape").clone();
    let mut absent_shape = absent.as_object().expect("absent shape").clone();
    assert_eq!(
        foreign_shape.remove("error"),
        Some(json!(format!("note not found: {note_id}")))
    );
    assert_eq!(
        absent_shape.remove("error"),
        Some(json!(format!("note not found: {absent_id}")))
    );
    assert_eq!(
        foreign_shape, absent_shape,
        "cross-project and unknown note IDs must be indistinguishable"
    );
    assert!(foreign["events"].as_array().unwrap().is_empty());
    assert!(foreign["history_start"].is_null());
    assert!(foreign["next_cursor"].is_null());
}

#[tokio::test]
async fn history_returns_empty_migration_cutover_for_live_pre_ledger_note() {
    let harness = McpTestHarness::new().await;
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;
    // The legacy repository create path predates the ledger: the note lives
    // without any retained revision events.
    let note = common::create_test_note(harness.db(), &project.id).await;

    let response = dispatch(
        &harness,
        "memory_history",
        json!({"project": project.slug(), "note_id": note.id}),
    )
    .await;
    assert!(response["error"].is_null(), "cutover failed: {response}");
    assert!(response["events"].as_array().unwrap().is_empty());
    assert!(response["next_cursor"].is_null());
    assert_eq!(response["history_start"], "migration_cutover");
}

#[tokio::test]
async fn history_retains_deleted_note_events_only_for_audit_authorized_callers() {
    let harness = McpTestHarness::new().await;
    let (project, note_id, created_revision, _) = seed_two_revision_note(&harness).await;

    let deleted = dispatch_as(
        owner_context(),
        &harness,
        "memory_delete",
        json!({
            "project": project.slug(),
            "identifier": note_id,
            "reason": "delete reader fixture"
        }),
    )
    .await;
    assert!(deleted["error"].is_null(), "delete failed: {deleted}");

    // The trusted local path (no user context) is audit-authorized.
    let retained = dispatch(
        &harness,
        "memory_history",
        json!({"project": project.slug(), "note_id": note_id}),
    )
    .await;
    assert!(retained["error"].is_null(), "retained history: {retained}");
    assert_eq!(retained["history_start"], "ledger");
    let events = retained["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["event_kind"], "deleted");
    assert_eq!(events[0]["note_seq"], 3);
    assert_eq!(events[0]["content_before"], "alpha\nBETA\n");
    assert!(events[0]["content_after"].is_null());
    assert_eq!(events[2]["revision_id"], created_revision);

    // An admin user is audit-authorized as well.
    let admin_id = seed_user(&harness, 7_301_001, "audit-admin", true).await;
    let admin_view = dispatch_as_user(
        &harness,
        admin_id,
        "memory_history",
        json!({"project": project.slug(), "note_id": note_id}),
    )
    .await;
    assert!(admin_view["error"].is_null(), "admin history: {admin_view}");
    assert_eq!(admin_view["events"].as_array().unwrap().len(), 3);

    // An authenticated non-admin gets uniform non-disclosure instead.
    let user_id = seed_user(&harness, 7_301_002, "plain-user", false).await;
    let denied = dispatch_as_user(
        &harness,
        user_id,
        "memory_history",
        json!({"project": project.slug(), "note_id": note_id}),
    )
    .await;
    assert_eq!(denied["error"], format!("note not found: {note_id}"));
    assert!(denied["events"].as_array().unwrap().is_empty());
    assert!(denied["history_start"].is_null());
}

// ── memory_diff ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn diff_renders_deterministic_unified_diff_from_explicit_snapshots() {
    let harness = McpTestHarness::new().await;
    let (project, note_id, created_revision, updated_revision) =
        seed_two_revision_note(&harness).await;

    let args = json!({
        "project": project.slug(),
        "note_id": note_id,
        "from_revision_id": created_revision,
        "to_revision_id": updated_revision,
    });
    let response = dispatch(&harness, "memory_diff", args.clone()).await;
    assert!(response["error"].is_null(), "diff failed: {response}");
    assert_eq!(
        response["from"],
        json!({
            "revision_id": created_revision,
            "note_seq": 1,
            "event_kind": "created",
            "created_at": response["from"]["created_at"],
        })
    );
    assert_eq!(
        response["to"],
        json!({
            "revision_id": updated_revision,
            "note_seq": 2,
            "event_kind": "updated",
            "created_at": response["to"]["created_at"],
        })
    );
    // The create-before edge coerces the from snapshot to empty, so the
    // inclusive span renders the full content as additions.
    assert_eq!(
        response["diff"],
        "--- from\n+++ to\n@@ -0,0 +1,2 @@\n+alpha\n+BETA\n"
    );
    assert!(
        response["intervening_events"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Rendering is deterministic across repeated calls.
    let repeat = dispatch(&harness, "memory_diff", args).await;
    assert_eq!(repeat["diff"], response["diff"]);

    // The delete-after edge coerces the to snapshot to empty: the inclusive
    // span from the update renders the pre-update content as a full removal.
    let deleted = dispatch_as(
        owner_context(),
        &harness,
        "memory_delete",
        json!({
            "project": project.slug(),
            "identifier": note_id,
            "reason": "delete reader fixture"
        }),
    )
    .await;
    assert!(deleted["error"].is_null(), "delete failed: {deleted}");
    let events = revision_repo(&harness)
        .revision_events_for_note(&project.id, &note_id)
        .await
        .expect("load revisions");
    let deleted_revision = events[2].id.clone();
    assert_eq!(events[2].event_kind, "deleted");

    let removal = dispatch(
        &harness,
        "memory_diff",
        json!({
            "project": project.slug(),
            "note_id": note_id,
            "from_revision_id": updated_revision,
            "to_revision_id": deleted_revision,
        }),
    )
    .await;
    assert!(removal["error"].is_null(), "removal diff: {removal}");
    assert_eq!(removal["to"]["event_kind"], "deleted");
    assert_eq!(
        removal["diff"],
        "--- from\n+++ to\n@@ -1,2 +0,0 @@\n-alpha\n-beta\n"
    );
}

#[tokio::test]
async fn diff_includes_intervening_non_content_events() {
    let harness = McpTestHarness::new().await;
    let (project, note_id, created_revision, _) = seed_two_revision_note(&harness).await;
    let confidence_revision = seed_confidence_revision(&harness, &project.id, &note_id, None).await;

    // A second edit becomes the newest content-bearing endpoint (seq 3 wait:
    // the confidence revision took seq 3? No — sequences allocate per event;
    // confirm ordering through the repository).
    let edited = dispatch_as(
        owner_context(),
        &harness,
        "memory_edit",
        json!({
            "project": project.slug(),
            "identifier": note_id,
            "operation": "find_replace",
            "find_text": "BETA",
            "content": "GAMMA",
            "reason": "second edit"
        }),
    )
    .await;
    assert!(edited["error"].is_null(), "second edit failed: {edited}");

    let events = revision_repo(&harness)
        .revision_events_for_note(&project.id, &note_id)
        .await
        .expect("load revisions");
    assert_eq!(events.len(), 4);
    assert_eq!(events[1].event_kind, "updated");
    assert_eq!(events[2].event_kind, "confidence_changed");
    assert_eq!(events[3].event_kind, "updated");
    let newest_revision = events[3].id.clone();

    let response = dispatch(
        &harness,
        "memory_diff",
        json!({
            "project": project.slug(),
            "note_id": note_id,
            "from_revision_id": created_revision,
            "to_revision_id": newest_revision,
        }),
    )
    .await;
    assert!(response["error"].is_null(), "diff failed: {response}");
    assert_eq!(
        response["diff"],
        "--- from\n+++ to\n@@ -0,0 +1,2 @@\n+alpha\n+GAMMA\n"
    );
    let intervening = response["intervening_events"].as_array().unwrap();
    assert_eq!(intervening.len(), 1, "only the confidence event intervenes");
    let event = &intervening[0];
    assert_event_keys(event);
    assert_eq!(event["revision_id"], confidence_revision);
    assert_eq!(event["event_kind"], "confidence_changed");
    assert_eq!(event["note_seq"], 3);
    assert!(event["content_before"].is_null());
    assert!(event["content_after"].is_null());
    assert_eq!(event["confidence_before"], 0.5);
    assert_eq!(event["confidence_after"], 0.9);
    assert_eq!(event["reason"], "bump confidence");
}

#[tokio::test]
async fn diff_rejects_non_content_endpoints_in_both_positions() {
    let harness = McpTestHarness::new().await;
    let (project, note_id, created_revision, updated_revision) =
        seed_two_revision_note(&harness).await;
    let confidence_revision = seed_confidence_revision(&harness, &project.id, &note_id, None).await;

    let from_position = dispatch(
        &harness,
        "memory_diff",
        json!({
            "project": project.slug(),
            "note_id": note_id,
            "from_revision_id": confidence_revision,
            "to_revision_id": updated_revision,
        }),
    )
    .await;
    assert_eq!(
        from_position["error"],
        "invalid parameters: field: from_revision_id, message: revision is not a content-bearing event"
    );
    assert!(from_position["from"].is_null());
    assert!(from_position["to"].is_null());
    assert_eq!(from_position["diff"], "");

    let to_position = dispatch(
        &harness,
        "memory_diff",
        json!({
            "project": project.slug(),
            "note_id": note_id,
            "from_revision_id": created_revision,
            "to_revision_id": confidence_revision,
        }),
    )
    .await;
    assert_eq!(
        to_position["error"],
        "invalid parameters: field: to_revision_id, message: revision is not a content-bearing event"
    );
}

#[tokio::test]
async fn diff_validates_existence_order_and_scope_without_disclosure() {
    let harness = McpTestHarness::new().await;
    let (project, note_id, created_revision, updated_revision) =
        seed_two_revision_note(&harness).await;
    let (other_project, other_note_id, other_created, _) = seed_two_revision_note(&harness).await;
    let _ = other_note_id;

    // Unknown endpoint IDs in either position are plain not-found.
    let absent = uuid::Uuid::now_v7().to_string();
    for (from, to, named) in [
        (absent.as_str(), updated_revision.as_str(), absent.as_str()),
        (created_revision.as_str(), absent.as_str(), absent.as_str()),
    ] {
        let response = dispatch(
            &harness,
            "memory_diff",
            json!({
                "project": project.slug(),
                "note_id": note_id,
                "from_revision_id": from,
                "to_revision_id": to,
            }),
        )
        .await;
        assert_eq!(response["error"], format!("revision not found: {named}"));
    }

    // A foreign note's revision is indistinguishable from an unknown one.
    let foreign = dispatch(
        &harness,
        "memory_diff",
        json!({
            "project": project.slug(),
            "note_id": note_id,
            "from_revision_id": other_created,
            "to_revision_id": updated_revision,
        }),
    )
    .await;
    assert_eq!(
        foreign["error"],
        format!("revision not found: {other_created}"),
        "foreign-note revisions must not be disclosed"
    );
    let _ = other_project;

    // The from endpoint must be strictly older than the to endpoint.
    let reversed = dispatch(
        &harness,
        "memory_diff",
        json!({
            "project": project.slug(),
            "note_id": note_id,
            "from_revision_id": updated_revision,
            "to_revision_id": created_revision,
        }),
    )
    .await;
    assert_eq!(
        reversed["error"],
        "invalid parameters: field: from_revision_id, message: from_revision_id must identify an older revision than to_revision_id"
    );

    let same = dispatch(
        &harness,
        "memory_diff",
        json!({
            "project": project.slug(),
            "note_id": note_id,
            "from_revision_id": created_revision,
            "to_revision_id": created_revision,
        }),
    )
    .await;
    assert_eq!(
        same["error"],
        "invalid parameters: field: from_revision_id, message: from_revision_id must identify an older revision than to_revision_id"
    );
}

// ── memory_session_diff ──────────────────────────────────────────────────────

/// Seed an epic/task/session/task-run fixture in an existing project.
async fn seed_session_fixture(
    harness: &McpTestHarness,
    project: &djinn_core::models::Project,
) -> (
    djinn_core::models::Epic,
    djinn_core::models::Task,
    djinn_core::models::SessionRecord,
) {
    let epic = common::create_test_epic(harness.db(), &project.id).await;
    let task = common::create_test_task(harness.db(), &project.id, &epic.id).await;
    let session = common::create_test_session(harness.db(), &project.id, &task.id).await;
    (epic, task, session)
}

/// Seed a project whose session/task-run execution produced a `created`
/// revision and a `confidence_changed` revision for one note. Returns
/// `(project, task, session, task_run_id, note_id)`.
async fn seed_executed_revision_fixture(
    harness: &McpTestHarness,
) -> (
    djinn_core::models::Project,
    djinn_core::models::Task,
    djinn_core::models::SessionRecord,
    String,
    String,
) {
    let (project, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let (_epic, task, session) = seed_session_fixture(harness, &project).await;
    let task_run = TaskRunRepository::new(harness.db().clone())
        .create(CreateTaskRunParams {
            id: &uuid::Uuid::now_v7().to_string(),
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "manual",
            status: None,
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .expect("create task run");

    let context = owner_context().with_execution_provenance(
        Some(session.id.clone()),
        Some(task.id.clone()),
        Some(task_run.id.clone()),
    );
    let created = dispatch_as(
        context,
        harness,
        "memory_write",
        json!({
            "project": project.slug(),
            "title": "Executed fixture note",
            "content": "executed\n",
            "type": "reference",
            "reason": "create executed fixture"
        }),
    )
    .await;
    assert!(created["error"].is_null(), "write failed: {created}");
    let note_id = created["id"].as_str().expect("note id").to_owned();
    seed_confidence_revision(
        harness,
        &project.id,
        &note_id,
        Some((&session.id, &task.id, &task_run.id)),
    )
    .await;
    (project, task, session, task_run.id, note_id)
}

#[tokio::test]
async fn session_diff_enforces_exactly_one_selector_and_bounds() {
    let harness = McpTestHarness::new().await;
    let (project, task, session, task_run_id, _note) =
        seed_executed_revision_fixture(&harness).await;
    let _ = task;

    let neither = dispatch(
        &harness,
        "memory_session_diff",
        json!({"project": project.slug()}),
    )
    .await;
    assert_eq!(
        neither["error"],
        "invalid parameters: field: session_id, message: exactly one of session_id or task_run_id must be provided"
    );

    let both = dispatch(
        &harness,
        "memory_session_diff",
        json!({
            "project": project.slug(),
            "session_id": session.id,
            "task_run_id": task_run_id,
        }),
    )
    .await;
    assert_eq!(
        both["error"],
        "invalid parameters: field: task_run_id, message: exactly one of session_id or task_run_id must be provided"
    );

    for limit in [0, -1, 501] {
        let response = dispatch(
            &harness,
            "memory_session_diff",
            json!({"project": project.slug(), "session_id": session.id, "limit": limit}),
        )
        .await;
        assert_eq!(
            response["error"],
            "invalid parameters: field: limit, message: limit must be between 1 and 500",
            "limit {limit} must be rejected at the public boundary"
        );
    }

    let cursor = dispatch(
        &harness,
        "memory_session_diff",
        json!({"project": project.slug(), "session_id": session.id, "before_cursor": "bogus"}),
    )
    .await;
    assert_eq!(
        cursor["error"],
        "invalid parameters: field: before_cursor, message: invalid cursor"
    );
}

#[tokio::test]
async fn session_diff_returns_all_kinds_newest_first_with_cursor_pagination() {
    let harness = McpTestHarness::new().await;
    let (project, task, session, task_run_id, note_id) =
        seed_executed_revision_fixture(&harness).await;

    for (selector, id) in [
        ("session_id", session.id.clone()),
        ("task_run_id", task_run_id.clone()),
    ] {
        let response = dispatch(
            &harness,
            "memory_session_diff",
            json!({"project": project.slug(), selector: id}),
        )
        .await;
        assert!(response["error"].is_null(), "{selector} failed: {response}");
        assert!(response["next_cursor"].is_null());
        let events = response["events"].as_array().unwrap();
        assert_eq!(events.len(), 2, "{selector} must return every event kind");
        // Newest-first by (created_at, id): the confidence event is newer.
        assert_eq!(events[0]["event_kind"], "confidence_changed");
        assert_eq!(events[1]["event_kind"], "created");
        for event in events {
            assert_event_keys(event);
            assert_eq!(event["session_id"], session.id);
            assert_eq!(event["task_id"], task.id);
            assert_eq!(event["task_run_id"], task_run_id);
            assert_eq!(event["note_id"], note_id);
            assert_eq!(event["content_redacted"], false);
        }
        assert_eq!(events[1]["content_after"], "executed\n");
    }

    // Bounded cursor pagination: page 1 returns the newest event only.
    let first = dispatch(
        &harness,
        "memory_session_diff",
        json!({"project": project.slug(), "session_id": session.id, "limit": 1}),
    )
    .await;
    assert!(first["error"].is_null(), "first page: {first}");
    assert_eq!(first["events"].as_array().unwrap().len(), 1);
    assert_eq!(first["events"][0]["event_kind"], "confidence_changed");
    let cursor = first["next_cursor"].as_str().expect("cursor").to_owned();

    let second = dispatch(
        &harness,
        "memory_session_diff",
        json!({
            "project": project.slug(),
            "session_id": session.id,
            "limit": 1,
            "before_cursor": cursor,
        }),
    )
    .await;
    assert!(second["error"].is_null(), "second page: {second}");
    assert_eq!(second["events"].as_array().unwrap().len(), 1);
    assert_eq!(second["events"][0]["event_kind"], "created");
    assert!(second["next_cursor"].is_null());

    // The note-history cursor must not replay against the session view.
    let history = dispatch(
        &harness,
        "memory_history",
        json!({"project": project.slug(), "note_id": note_id, "limit": 1}),
    )
    .await;
    assert!(history["error"].is_null(), "history page: {history}");
    let history_cursor = history["next_cursor"]
        .as_str()
        .expect("history cursor")
        .to_owned();
    let replayed = dispatch(
        &harness,
        "memory_session_diff",
        json!({
            "project": project.slug(),
            "session_id": session.id,
            "before_cursor": history_cursor,
        }),
    )
    .await;
    assert_eq!(
        replayed["error"],
        "invalid parameters: field: before_cursor, message: invalid cursor"
    );
}

#[tokio::test]
async fn session_diff_redacts_bodies_without_note_read_permission() {
    let harness = McpTestHarness::new().await;
    let (project, _task, session, _run, _note) = seed_executed_revision_fixture(&harness).await;

    let user_id = seed_user(&harness, 7_301_101, "reader-less-user", false).await;
    let redacted = dispatch_as_user(
        &harness,
        user_id,
        "memory_session_diff",
        json!({"project": project.slug(), "session_id": session.id}),
    )
    .await;
    assert!(redacted["error"].is_null(), "redacted view: {redacted}");
    let events = redacted["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    for event in events {
        assert_eq!(event["content_redacted"], true);
        assert!(event["content_before"].is_null());
        assert!(event["content_after"].is_null());
        // Metadata remains visible under redaction.
        assert!(event["event_kind"].is_string());
        assert!(event["reason"].is_string());
        assert_eq!(event["session_id"], session.id);
    }

    // The trusted path and admins receive full bodies.
    let full = dispatch(
        &harness,
        "memory_session_diff",
        json!({"project": project.slug(), "session_id": session.id}),
    )
    .await;
    assert_eq!(full["events"][1]["content_after"], "executed\n");
    assert_eq!(full["events"][1]["content_redacted"], false);

    let admin_id = seed_user(&harness, 7_301_102, "reader-admin", true).await;
    let admin_view = dispatch_as_user(
        &harness,
        admin_id,
        "memory_session_diff",
        json!({"project": project.slug(), "session_id": session.id}),
    )
    .await;
    assert_eq!(admin_view["events"][1]["content_after"], "executed\n");
    assert_eq!(admin_view["events"][1]["content_redacted"], false);
}

#[tokio::test]
async fn session_diff_scopes_selectors_to_the_caller_project() {
    let harness = McpTestHarness::new().await;
    let (owner_project, _task, session, task_run_id, _note) =
        seed_executed_revision_fixture(&harness).await;
    let (other_project, _dir) = common::create_test_project_with_dir(harness.db()).await;

    let foreign_session = dispatch(
        &harness,
        "memory_session_diff",
        json!({"project": other_project.slug(), "session_id": session.id}),
    )
    .await;
    let absent_session_id = uuid::Uuid::now_v7().to_string();
    let absent_session = dispatch(
        &harness,
        "memory_session_diff",
        json!({"project": owner_project.slug(), "session_id": absent_session_id}),
    )
    .await;
    assert_eq!(
        foreign_session["error"],
        format!("session not found: {}", session.id),
        "cross-project sessions must not be disclosed"
    );
    assert_eq!(
        absent_session["error"],
        format!("session not found: {absent_session_id}"),
        "unknown sessions carry the identical not-found shape"
    );

    let foreign_run = dispatch(
        &harness,
        "memory_session_diff",
        json!({"project": other_project.slug(), "task_run_id": task_run_id}),
    )
    .await;
    assert_eq!(
        foreign_run["error"],
        format!("task run not found: {task_run_id}"),
        "cross-project task runs must not be disclosed"
    );
}

// ── No git fallback ──────────────────────────────────────────────────────────

#[tokio::test]
async fn restored_readers_expose_no_git_fallback() {
    let harness = McpTestHarness::new().await;
    let (project, note_id, created_revision, _) = seed_two_revision_note(&harness).await;

    // The retired file-era shapes (permalink/SHA selectors) are rejected at
    // argument decoding instead of reaching any git-backed path.
    let history_error = harness
        .call_tool(
            "memory_history",
            json!({"project": project.slug(), "permalink": "reference/reader-fixture-note"}),
        )
        .await
        .expect_err("legacy permalink selector must be rejected");
    assert!(
        format!("{history_error:#}").contains("unknown field `permalink`"),
        "unexpected error: {history_error:#}"
    );

    let diff_error = harness
        .call_tool(
            "memory_diff",
            json!({
                "project": project.slug(),
                "permalink": "reference/reader-fixture-note",
                "sha": "deadbeef",
            }),
        )
        .await
        .expect_err("legacy SHA selector must be rejected");
    let diff_error = format!("{diff_error:#}");
    assert!(diff_error.contains("unknown field `permalink`"));
    assert!(diff_error.contains("unknown field `sha`") || diff_error.contains("permalink"));

    // Successful responses carry only the approved ledger fields.
    let history = dispatch(
        &harness,
        "memory_history",
        json!({"project": project.slug(), "note_id": note_id}),
    )
    .await;
    let serialized = serde_json::to_string(&history).unwrap();
    for legacy_key in ["\"sha\"", "\"permalink\"", "\"git\"", "\"commit\""] {
        assert!(
            !serialized.contains(legacy_key),
            "history response leaks a legacy git field: {legacy_key}"
        );
    }

    let diff = dispatch(
        &harness,
        "memory_diff",
        json!({
            "project": project.slug(),
            "note_id": note_id,
            "from_revision_id": created_revision,
            "to_revision_id": created_revision,
        }),
    )
    .await;
    let serialized = serde_json::to_string(&diff).unwrap();
    for legacy_key in ["\"sha\"", "\"permalink\"", "\"git\"", "\"commit\""] {
        assert!(
            !serialized.contains(legacy_key),
            "diff response leaks a legacy git field: {legacy_key}"
        );
    }
}
