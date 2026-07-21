use djinn_agent::output_stash::{DurableOutputDetails, OutputStash, handle_stash_tool};
use djinn_agent::test_helpers::{
    persist_tool_results_before_compaction_for_test, test_persistent_dir,
};
use djinn_slot::host::PreCompactionToolResult;
use std::sync::Mutex;

fn result(id: &str, bytes: &str, turn: u64) -> PreCompactionToolResult {
    PreCompactionToolResult {
        tool_use_id: id.into(),
        tool_name: "shell".into(),
        content: bytes.into(),
        turn,
    }
}
fn view(stash: &Mutex<OutputStash>, id: &str) -> Result<String, String> {
    let args = serde_json::json!({"tool_use_id": id})
        .as_object()
        .unwrap()
        .clone();
    handle_stash_tool(stash, "output_view", Some(&args))
}

#[test]
fn preclear_stash_is_atomic() {
    let root = test_persistent_dir("compaction-hardening-atomic-");
    let mut stash = OutputStash::with_session_id_and_durable_root("owner", root.clone());
    stash
        .insert_with_metadata(
            "large".into(),
            "shell".into(),
            "large stored\n".into(),
            DurableOutputDetails {
                turn: 9,
                result_kind: "shell_stdout".into(),
                original_chars: 999,
                stored_chars: 13,
                completeness: "partial-spill".into(),
            },
        )
        .unwrap();
    let overflow = "x".repeat(600);
    let results = vec![
        result("small", "unstashed small inline\n", 1),
        result("large", "must not overwrite", 2),
        result("truncated", "partial retained bytes\n", 3),
        result("overflow-micro", &overflow, 4),
    ];
    assert_eq!(
        persist_tool_results_before_compaction_for_test(&mut stash, &results)
            .unwrap()
            .len(),
        4
    );
    persist_tool_results_before_compaction_for_test(&mut stash, &results).unwrap();
    let records = stash.list_durable_outputs().unwrap();
    assert_eq!(records.len(), 4);
    let large = records.iter().find(|r| r.tool_use_id == "large").unwrap();
    assert_eq!(
        (
            large.turn,
            large.result_kind.as_str(),
            large.completeness.as_str()
        ),
        (9, "shell_stdout", "partial-spill")
    );
    stash.clear();
    assert!(
        stash
            .view("small", 0, 10)
            .unwrap()
            .contains("unstashed small inline")
    );
    assert!(stash.view("overflow-micro", 0, 10).unwrap().contains("xxx"));
    stash.set_fail_durable_writes_for_test(true);
    let inline = "must remain inline after durable failure";
    assert!(
        persist_tool_results_before_compaction_for_test(&mut stash, &[result("failed", inline, 5)])
            .unwrap_err()
            .contains("injected durable output write failure")
    );
    assert!(stash.view("failed", 0, 10).is_err());
    assert_eq!(inline, "must remain inline after durable failure");
}

#[test]
fn survives_modes_reload_and_enforces_session() {
    let root = test_persistent_dir("compaction-hardening-reload-");
    let modes = [
        ("micro", "micro retained bytes\n", 1),
        ("partial", "partial retained bytes\n", 2),
        ("full", "full retained bytes\n", 3),
        ("fallback", "fallback retained bytes\n", 4),
    ];
    let mut original = OutputStash::with_session_id_and_durable_root("trusted-owner", root.clone());
    let results: Vec<_> = modes
        .iter()
        .map(|(mode, bytes, turn)| result(&format!("{mode}-tool"), bytes, *turn))
        .collect();
    persist_tool_results_before_compaction_for_test(&mut original, &results).unwrap();
    original.clear();
    drop(original);
    let owner = Mutex::new(OutputStash::with_session_id_and_durable_root(
        "trusted-owner",
        root.clone(),
    ));
    let listed: Vec<serde_json::Value> =
        serde_json::from_str(&handle_stash_tool(&owner, "output_list", None).unwrap()).unwrap();
    assert_eq!(listed.len(), 4);
    for (mode, bytes, turn) in modes {
        let id = format!("{mode}-tool");
        let record = listed.iter().find(|r| r["tool_use_id"] == id).unwrap();
        assert_eq!(record["turn"], turn);
        assert_eq!(record["result_kind"], "tool_result");
        assert_eq!(record["original_chars"], bytes.chars().count());
        assert_eq!(record["stored_chars"], bytes.chars().count());
        assert_eq!(record["completeness"], "complete");
        assert!(view(&owner, &id).unwrap().contains(bytes));
    }
    let foreign = Mutex::new(OutputStash::with_session_id_and_durable_root(
        "other-trusted-session",
        root.clone(),
    ));
    assert_eq!(
        handle_stash_tool(&foreign, "output_list", None).unwrap(),
        "[]"
    );
    for (mode, _, _) in modes {
        assert!(view(&foreign, &format!("{mode}-tool")).is_err());
    }
    assert!(view(&owner, "micro-tool").is_ok());
}
