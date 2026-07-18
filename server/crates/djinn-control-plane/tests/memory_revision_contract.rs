//! Aggregate contract for the shared memory-revision fixture and pinned MCP schemas.
//!
//! Focused live-dispatch validation belongs to the MCP mutation contract tests.
//! This target deliberately pins the cross-writer fixture vocabulary and checks
//! both published schema projections instead of regenerating either artifact.

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::auth_context::SESSION_USER_ID;
use djinn_core::events::EventBus;
use djinn_db::{ProjectRepository, UserRepository};
use serde_json::Value;
use sqlx::{Postgres, QueryBuilder};

const CONTRACT: &str = include_str!("fixtures/memory_revision_contract.json");
const PROVIDER_PROJECTION: &str = include_str!(
    "../../djinn-provider/tests/fixtures/tool_schema_projection/builtin/djinn_mcp_server.json"
);
const SERVER_SCHEMA_SNAPSHOT: &str = include_str!(
    "../../../src/server/tests/snapshots/djinn_server__server__tests__tool_schemas__mcp_tools_schema.snap"
);

fn contract() -> Value {
    serde_json::from_str(CONTRACT).expect("memory revision contract fixture is valid JSON")
}

/// Seed fixed rows by immutable INSERT only: no writer or schema relaxation is
/// involved, and the fixture owns every value projected by the public readers.
async fn seed_reader_contract(harness: &McpTestHarness, fixture: &Value) {
    let seed = &fixture["reader_fixture"];
    let project_id = fixture["ids"]["project_id"]
        .as_str()
        .expect("fixture project id");
    ProjectRepository::new(harness.db().clone(), EventBus::noop())
        .create_with_id(
            project_id,
            "revision-contract",
            "fixture",
            "revision-contract",
        )
        .await
        .expect("seed fixed project");
    for note in seed["live_notes"].as_array().expect("fixture live notes") {
        let mut insert = QueryBuilder::<Postgres>::new(
            "INSERT INTO notes (id, project_id, permalink, title, file_path, storage, note_type, folder, status, tags, content, scope_paths, confidence) VALUES (",
        );
        {
            let mut values = insert.separated(", ");
            values.push_bind(note["id"].as_str().expect("note id"));
            values.push_bind(project_id);
            values.push_bind(note["permalink"].as_str().expect("permalink"));
            values.push_bind(note["title"].as_str().expect("title"));
            values.push("'', 'db'");
            values.push_bind(
                note.get("note_type")
                    .and_then(Value::as_str)
                    .unwrap_or("reference"),
            );
            values.push_bind(
                note.get("folder")
                    .and_then(Value::as_str)
                    .unwrap_or("reference"),
            );
            values.push("'active', '[]'");
            values.push_bind(note["content"].as_str().expect("content"));
            values.push("'[]'");
            values.push_bind(note["confidence"].as_f64().expect("confidence"));
        }
        insert
            .push(")")
            .build()
            .execute(harness.db().pool())
            .await
            .expect("seed fixed live note");
    }
    for event in seed["events"].as_array().expect("fixture events") {
        let mut insert = QueryBuilder::<Postgres>::new(
            "INSERT INTO note_revision_events (id, project_id, note_id, note_seq, event_kind, content_before, content_after, confidence_before, confidence_after, actor_kind, actor_id, subsystem, session_id, task_id, task_run_id, reason, created_at) VALUES (",
        );
        {
            let mut values = insert.separated(", ");
            values.push_bind(event["id"].as_str().expect("revision id"));
            values.push_bind(project_id);
            values.push_bind(event["note_id"].as_str().expect("event note id"));
            values.push_bind(event["note_seq"].as_i64().expect("note sequence"));
            values.push_bind(event["event_kind"].as_str().expect("event kind"));
            values.push_bind(event.get("content_before").and_then(Value::as_str));
            values.push_bind(event.get("content_after").and_then(Value::as_str));
            values.push_bind(event.get("confidence_before").and_then(Value::as_f64));
            values.push_bind(event.get("confidence_after").and_then(Value::as_f64));
            values.push_bind(event["actor_kind"].as_str().expect("actor kind"));
            values.push_bind(event.get("actor_id").and_then(Value::as_str));
            values.push_bind(event.get("subsystem").and_then(Value::as_str));
            values.push_bind(event.get("session_id").and_then(Value::as_str));
            values.push_bind(event.get("task_id").and_then(Value::as_str));
            values.push_bind(event.get("task_run_id").and_then(Value::as_str));
            values.push_bind(event["reason"].as_str().expect("reason"));
            values.push_bind(event["created_at"].as_str().expect("timestamp"));
        }
        insert
            .push(")")
            .build()
            .execute(harness.db().pool())
            .await
            .expect("append immutable fixed revision event");
    }

    let context = &seed["session_context"];
    let task_id = context["task_id"].as_str().expect("fixed task id");
    let session_id = context["session_id"].as_str().expect("fixed session id");
    let task_run_id = context["task_run_id"].as_str().expect("fixed task run id");
    let mut task_insert = QueryBuilder::<Postgres>::new(
        "INSERT INTO tasks (id, project_id, short_id, title, description, design, labels, acceptance_criteria, memory_refs) VALUES (",
    );
    {
        let mut values = task_insert.separated(", ");
        values.push_bind(task_id);
        values.push_bind(project_id);
        values.push("'fixed-task', 'Fixed contract task', '', '', '[]', '[]', '[]'");
    }
    task_insert
        .push(")")
        .build()
        .execute(harness.db().pool())
        .await
        .expect("seed fixed task");

    let mut task_run_insert = QueryBuilder::<Postgres>::new(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status, started_at) VALUES (",
    );
    {
        let mut values = task_run_insert.separated(", ");
        values.push_bind(task_run_id);
        values.push_bind(project_id);
        values.push_bind(task_id);
        values.push("'manual', 'running', '2026-02-04T05:06:07.000Z'");
    }
    task_run_insert
        .push(")")
        .build()
        .execute(harness.db().pool())
        .await
        .expect("seed fixed task run");

    let mut session_insert = QueryBuilder::<Postgres>::new(
        "INSERT INTO sessions (id, project_id, task_id, task_run_id, model_id, agent_type, started_at, status) VALUES (",
    );
    {
        let mut values = session_insert.separated(", ");
        values.push_bind(session_id);
        values.push_bind(project_id);
        values.push_bind(task_id);
        values.push_bind(task_run_id);
        values.push("'fixture-model', 'worker', '2026-02-04T05:06:07.000Z', 'active'");
    }
    session_insert
        .push(")")
        .build()
        .execute(harness.db().pool())
        .await
        .expect("seed fixed session");

    // Second session/task-run used by the equal-time session-diff and extracted-audit
    // scenarios. These are separate from the history/diff session so the session-diff
    // events are isolated from the tk95 envelope.
    let sd_session = context["session_diff_session_id"]
        .as_str()
        .expect("fixed session-diff session id");
    let sd_task_run = context["session_diff_task_run_id"]
        .as_str()
        .expect("fixed session-diff task run id");
    let mut session_diff_task_run_insert = QueryBuilder::<Postgres>::new(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status, started_at) VALUES (",
    );
    {
        let mut values = session_diff_task_run_insert.separated(", ");
        values.push_bind(sd_task_run);
        values.push_bind(project_id);
        values.push_bind(task_id);
        values.push("'manual', 'running', '2026-02-04T05:06:05.000Z'");
    }
    session_diff_task_run_insert
        .push(")")
        .build()
        .execute(harness.db().pool())
        .await
        .expect("seed session-diff task run");

    let mut session_diff_insert = QueryBuilder::<Postgres>::new(
        "INSERT INTO sessions (id, project_id, task_id, task_run_id, model_id, agent_type, started_at, status) VALUES (",
    );
    {
        let mut values = session_diff_insert.separated(", ");
        values.push_bind(sd_session);
        values.push_bind(project_id);
        values.push_bind(task_id);
        values.push_bind(sd_task_run);
        values.push("'fixture-model', 'worker', '2026-02-04T05:06:05.000Z', 'active'");
    }
    session_diff_insert
        .push(")")
        .build()
        .execute(harness.db().pool())
        .await
        .expect("seed session-diff session");
}

async fn dispatch(harness: &McpTestHarness, tool: &str, args: Value) -> Value {
    harness
        .call_tool(tool, args)
        .await
        .unwrap_or_else(|error| panic!("{tool} dispatch failed: {error}"))
}

fn named_tool<'a>(tools: &'a [Value], name: &str) -> &'a Value {
    tools
        .iter()
        .find(|tool| tool["name"] == name)
        .unwrap_or_else(|| panic!("missing {name} schema"))
}

fn assert_reason_is_required(schema: &Value, required_key: &str, properties_key: &str) {
    let required = schema[required_key]
        .as_array()
        .unwrap_or_else(|| panic!("{required_key} must be an array"));
    assert!(
        required.iter().any(|field| field == "reason"),
        "reason must be required"
    );
    let properties = schema[properties_key]
        .as_object()
        .unwrap_or_else(|| panic!("{properties_key} must be an object"));
    assert!(properties.contains_key("reason"), "reason must be exposed");
    assert_eq!(
        schema["additionalProperties"], false,
        "caller-supplied attribution must be rejected"
    );
    for untrusted in [
        "actor",
        "actor_id",
        "actor_kind",
        "subsystem",
        "session_id",
        "task_id",
        "task_run_id",
        "provenance",
    ] {
        assert!(
            !properties.contains_key(untrusted),
            "trusted attribution field {untrusted} must not be a caller input"
        );
    }
}

#[test]
fn fixture_pins_all_writers_events_and_semantic_noops() {
    let fixture = contract();
    let rows = fixture["repository_expected_rows"]
        .as_array()
        .expect("fixture rows");

    assert_eq!(
        fixture["allowed_event_kinds"],
        serde_json::json!([
            "create",
            "update",
            "delete",
            "confidence_bump",
            "extraction_skipped"
        ])
    );
    assert_eq!(
        fixture["registered_subsystems"],
        serde_json::json!(["mcp", "dedup", "consolidation", "enrichment", "extraction"])
    );

    for row in rows {
        let reason = row["reason"].as_str().expect("row reason");
        assert!(
            !reason.is_empty() && reason == reason.trim(),
            "trimmed reason"
        );
        assert_eq!(
            row["reason_input"].as_str().expect("reason input").trim(),
            reason,
            "fixture pins canonical reason after Unicode-aware trimming"
        );
    }

    let has = |event_kind: &str, subsystem: &str| {
        rows.iter()
            .any(|row| row["event_kind"] == event_kind && row["subsystem"] == subsystem)
    };
    assert!(has("create", "mcp"));
    assert!(has("update", "enrichment"));
    assert!(has("confidence_bump", "enrichment"));
    assert!(has("create", "dedup"));
    assert!(has("confidence_bump", "dedup"));
    assert!(has("create", "consolidation"));
    assert!(has("update", "consolidation"));
    assert!(has("delete", "mcp"));
    assert!(has("extraction_skipped", "extraction"));

    let already_known = rows
        .iter()
        .find(|row| row["decision"] == "AlreadyKnown")
        .expect("AlreadyKnown revision");
    assert_eq!(already_known["event_kind"], "confidence_bump");
    assert!(already_known["before_snapshot"].is_null());
    assert!(already_known["after_snapshot"].is_null());

    let merged = rows
        .iter()
        .find(|row| row["decision"] == "MergeIntoExisting")
        .expect("MergeIntoExisting revision");
    assert_eq!(merged["event_kind"], "update");
    assert_ne!(merged["before_snapshot"], merged["after_snapshot"]);
    assert!(merged["confidence_after"].as_f64() > merged["confidence_before"].as_f64());

    let unscoped = rows
        .iter()
        .find(|row| row["note_id"] == fixture["ids"]["notes"]["unscoped"])
        .expect("unscoped revision");
    for field in ["session_id", "task_id", "task_run_id"] {
        assert!(
            unscoped[field].is_null(),
            "{field} is null without execution context"
        );
    }

    let no_events = fixture["semantic_no_event_cases"]
        .as_array()
        .expect("semantic no-event cases");
    assert_eq!(no_events.len(), 2);
    for case in no_events {
        assert_eq!(
            case["expected_event_count"], 0,
            "{} requested no-op must not append a revision",
            case["name"]
        );
    }
}

#[tokio::test]
async fn fixed_fixture_pins_history_pages_pairwise_diff_and_deleted_history() {
    let fixture = contract();
    let harness = McpTestHarness::new().await;
    seed_reader_contract(&harness, &fixture).await;
    let reader = &fixture["reader_fixture"];
    let requests = &reader["requests"];
    let expected = &reader["expected"];

    let full = dispatch(&harness, "memory_history", requests["history_full"].clone()).await;
    assert_eq!(full, expected["history_full"]);

    let first_page = dispatch(
        &harness,
        "memory_history",
        requests["history_page_1"].clone(),
    )
    .await;
    assert_eq!(first_page, expected["history_page_1"]);

    let second_page = dispatch(
        &harness,
        "memory_history",
        requests["history_page_2"].clone(),
    )
    .await;
    assert_eq!(second_page, expected["history_page_2"]);

    let diff = dispatch(&harness, "memory_diff", requests["diff"].clone()).await;
    assert_eq!(diff, expected["diff"]);

    let deleted = dispatch(
        &harness,
        "memory_history",
        requests["deleted_history"].clone(),
    )
    .await;
    assert_eq!(deleted, expected["deleted_history"]);
}

#[tokio::test]
async fn fixed_fixture_pins_equal_time_session_diff_pages_and_extracted_audit() {
    let fixture = contract();
    let harness = McpTestHarness::new().await;
    seed_reader_contract(&harness, &fixture).await;
    let reader = &fixture["reader_fixture"];
    let requests = &reader["requests"];
    let expected = &reader["expected"];

    // Full session-diff: all five events ordered newest-first by
    // (created_at DESC, id DESC), exercising the equal-time tie-break.
    let session_full = dispatch(
        &harness,
        "memory_session_diff",
        requests["session_diff_full"].clone(),
    )
    .await;
    assert_eq!(session_full, expected["session_diff_full"]);

    // Cursor page 1: first two equal-time events + cursor into the boundary.
    let sd_page_1 = dispatch(
        &harness,
        "memory_session_diff",
        requests["session_diff_page_1"].clone(),
    )
    .await;
    assert_eq!(sd_page_1, expected["session_diff_page_1"]);

    // Cursor page 2: next two equal-time events.
    let sd_page_2 = dispatch(
        &harness,
        "memory_session_diff",
        requests["session_diff_page_2"].clone(),
    )
    .await;
    assert_eq!(sd_page_2, expected["session_diff_page_2"]);

    // Cursor page 3: final single event.
    let sd_page_3 = dispatch(
        &harness,
        "memory_session_diff",
        requests["session_diff_page_3"].clone(),
    )
    .await;
    assert_eq!(sd_page_3, expected["session_diff_page_3"]);

    // Readerless/redacted: a non-admin caller gets the same page shape but
    // with content bodies withheld and content_redacted = true.
    let readerless = UserRepository::new(harness.db().clone())
        .upsert_from_github(8_420_001, "session-diff-readerless", None, None)
        .await
        .expect("seed readerless user");
    let redacted = SESSION_USER_ID
        .scope(
            Some(readerless.id.clone()),
            dispatch(
                &harness,
                "memory_session_diff",
                requests["session_diff_page_1"].clone(),
            ),
        )
        .await;
    assert_eq!(redacted, expected["session_diff_redacted"]);

    // Extracted audit: a nonempty attributed underspecified finding.
    let audit = dispatch(
        &harness,
        "memory_extracted_audit",
        requests["extracted_audit"].clone(),
    )
    .await;
    assert_eq!(audit, expected["extracted_audit"]);
}

#[test]
fn server_and_provider_schemas_pin_reason_without_spoofable_attribution() {
    let provider: Vec<Value> =
        serde_json::from_str(PROVIDER_PROJECTION).expect("provider MCP projection is valid JSON");
    let snapshot_payload = SERVER_SCHEMA_SNAPSHOT
        .rsplit_once("---\n")
        .expect("insta snapshot payload delimiter")
        .1;
    let server: Vec<Value> =
        serde_json::from_str(snapshot_payload).expect("server MCP schema snapshot is valid JSON");

    for name in ["memory_write", "memory_edit", "memory_delete"] {
        let provider_schema = &named_tool(&provider, name)["inputSchema"];
        let server_schema = &named_tool(&server, name)["input_schema"];
        assert_reason_is_required(provider_schema, "required", "properties");
        assert_reason_is_required(server_schema, "required", "properties");
        assert_eq!(
            provider_schema["required"], server_schema["required"],
            "{name} required fields must agree in both pinned projections"
        );
    }
}
