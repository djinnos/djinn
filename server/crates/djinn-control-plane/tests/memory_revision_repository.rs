//! Shape contract for the deterministic memory revision repository fixture.
//!
//! This deliberately validates only the fixture's repository-facing contract;
//! MCP history/diff response shapes are owned by later reader epics.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;
use uuid::Uuid;

const CONTRACT: &str = include_str!("fixtures/memory_revision_contract.json");

fn object<'a>(value: &'a Value, context: &str) -> &'a serde_json::Map<String, Value> {
    value
        .as_object()
        .unwrap_or_else(|| panic!("{context} must be an object"))
}

fn string<'a>(row: &'a serde_json::Map<String, Value>, field: &str) -> &'a str {
    row.get(field)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("row field {field} must be a string"))
}

#[test]
fn memory_revision_contract_has_valid_repository_event_shapes_and_ordering() {
    let contract: Value = serde_json::from_str(CONTRACT).expect("fixture must be valid JSON");
    let contract = object(&contract, "contract");

    assert_eq!(contract["contract_version"], 1);
    assert!(
        contract["mcp_response_expectations"]["intentionally_separate_from_repository_rows"]
            .as_bool()
            .unwrap()
    );

    let allowed_kinds: HashSet<&str> = contract["allowed_event_kinds"]
        .as_array()
        .expect("allowed_event_kinds must be an array")
        .iter()
        .map(|value| value.as_str().expect("event kind must be a string"))
        .collect();
    let allowed_actors: HashSet<&str> = contract["allowed_actor_kinds"]
        .as_array()
        .expect("allowed_actor_kinds must be an array")
        .iter()
        .map(|value| value.as_str().expect("actor kind must be a string"))
        .collect();
    let registered_subsystems: HashSet<&str> = contract["registered_subsystems"]
        .as_array()
        .expect("registered_subsystems must be an array")
        .iter()
        .map(|value| value.as_str().expect("subsystem must be a string"))
        .collect();

    let rows = contract["repository_expected_rows"]
        .as_array()
        .expect("repository_expected_rows must be an array");
    assert!(!rows.is_empty());

    let mut revision_ids = HashSet::new();
    let mut sequences_by_note: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    let mut cursor_keys = Vec::new();
    let mut observed_event_kinds = HashSet::new();

    for value in rows {
        let row = object(value, "repository row");
        let id = string(row, "id");
        assert!(
            Uuid::parse_str(id).is_ok(),
            "revision ID {id} must be a UUID"
        );
        assert!(revision_ids.insert(id), "revision ID {id} must be unique");

        let event_kind = string(row, "event_kind");
        assert!(
            allowed_kinds.contains(event_kind),
            "unknown event kind {event_kind}"
        );
        observed_event_kinds.insert(event_kind);
        assert!(allowed_actors.contains(string(row, "actor_kind")));
        assert!(registered_subsystems.contains(string(row, "subsystem")));
        assert!(!string(row, "reason").is_empty());
        assert_eq!(string(row, "reason"), string(row, "reason_input").trim());
        assert_ne!(string(row, "reason"), string(row, "reason_input"));
        assert!(!string(row, "created_at").is_empty());
        cursor_keys.push((string(row, "created_at"), id));

        let note_id = row.get("note_id").and_then(Value::as_str);
        let sequence = row.get("sequence").and_then(Value::as_u64);
        match event_kind {
            "create" => {
                assert!(note_id.is_some());
                assert_eq!(row["before_snapshot"], Value::Null);
                assert!(row["after_snapshot"].is_object());
                assert!(row["confidence_before"].is_null());
                assert!(row["confidence_after"].is_number());
            }
            "update" => {
                assert!(note_id.is_some());
                assert!(row["before_snapshot"].is_object());
                assert!(row["after_snapshot"].is_object());
                assert!(row["confidence_before"].is_number());
                assert!(row["confidence_after"].is_number());
            }
            "delete" => {
                assert!(note_id.is_some());
                assert!(row["before_snapshot"].is_object());
                assert_eq!(row["after_snapshot"], Value::Null);
                assert!(row["confidence_before"].is_number());
                assert!(row["confidence_after"].is_null());
            }
            "confidence_bump" => {
                assert!(note_id.is_some());
                assert_eq!(row["before_snapshot"], Value::Null);
                assert_eq!(row["after_snapshot"], Value::Null);
                assert!(row["confidence_before"].is_number());
                assert!(row["confidence_after"].is_number());
            }
            "extraction_skipped" => {
                assert!(note_id.is_none());
                assert!(sequence.is_none());
                assert_eq!(row["before_snapshot"], Value::Null);
                assert_eq!(row["after_snapshot"], Value::Null);
                assert!(row["confidence_before"].is_null());
                assert!(row["confidence_after"].is_null());
            }
            _ => unreachable!("event kind was checked against the allowed set"),
        }

        if let (Some(note_id), Some(sequence)) = (note_id, sequence) {
            sequences_by_note.entry(note_id).or_default().push(sequence);
        }
    }

    assert_eq!(observed_event_kinds, allowed_kinds);
    for (note_id, sequences) in sequences_by_note {
        let expected: Vec<u64> = (1..=sequences.len() as u64).collect();
        assert_eq!(
            sequences, expected,
            "per-note sequence for {note_id} must begin at one and be monotonic"
        );
    }

    let mut sorted_cursor_keys = cursor_keys.clone();
    sorted_cursor_keys.sort_unstable();
    assert_eq!(
        cursor_keys, sorted_cursor_keys,
        "rows must be in cursor order"
    );

    let cursor_ids: Vec<&str> = contract["cursor_order"]["expected_revision_ids"]
        .as_array()
        .expect("cursor IDs must be an array")
        .iter()
        .map(|value| value.as_str().expect("cursor ID must be a string"))
        .collect();
    let row_ids: Vec<&str> = rows
        .iter()
        .map(|value| string(object(value, "repository row"), "id"))
        .collect();
    assert_eq!(cursor_ids, row_ids);
}
