//! Aggregate contract for the shared memory-revision fixture and pinned MCP schemas.
//!
//! Focused live-dispatch validation belongs to the MCP mutation contract tests.
//! This target deliberately pins the cross-writer fixture vocabulary and checks
//! both published schema projections instead of regenerating either artifact.

use serde_json::Value;

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
