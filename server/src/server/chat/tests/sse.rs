//! SSE payload tests.  The `input` field is opaque JSON so we inline
//! a representative per-tool-call shape — chat tools under the
//! chat-user-global refactor carry an explicit `project` argument.

use serde_json::json;

use super::super::ToolCallPayload;
use crate::server::chat::handler::sse_json_event;

#[test]
fn tool_call_sse_payload_includes_id_input_and_name() {
    let payload = ToolCallPayload {
        name: "read".to_string(),
        id: "call-123".to_string(),
        input: json!({"project": "alice/demo", "file_path": "README.md"}),
    };

    let event = sse_json_event("tool_call", &payload);
    let serialized = format!("{event:?}");

    assert!(serialized.contains("event: tool_call"));

    let value = serde_json::to_value(payload).expect("payload serializes");
    assert_eq!(value.get("name").and_then(|v| v.as_str()), Some("read"));
    assert_eq!(value.get("id").and_then(|v| v.as_str()), Some("call-123"));
    assert_eq!(
        value.get("input"),
        Some(&json!({"project": "alice/demo", "file_path": "README.md"}))
    );
}

#[test]
fn tool_call_payload_serialization_keeps_existing_keys_for_backward_compat() {
    let payload = ToolCallPayload {
        name: "memory_search".to_string(),
        id: "call-456".to_string(),
        input: json!({"query": "foo"}),
    };

    let value = serde_json::to_value(payload).expect("payload serializes");

    assert_eq!(
        value.get("name").and_then(|v| v.as_str()),
        Some("memory_search")
    );
    assert_eq!(value.get("id").and_then(|v| v.as_str()), Some("call-456"));
    assert_eq!(value.get("input"), Some(&json!({"query": "foo"})));
}
