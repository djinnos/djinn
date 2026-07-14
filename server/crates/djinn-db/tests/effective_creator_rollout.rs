//! Expand-phase inventory gate for every production task writer.
#[test]
fn inventoried_producers_reach_the_transactional_provenance_boundary() {
    let inventory: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/effective_creator_producers.json"))
            .expect("valid inventory");
    let writers = inventory["writers"].as_array().expect("writers");
    assert!(!writers.is_empty());
    let writes = include_str!("../src/repositories/task/writes.rs");
    assert!(writes.contains("let created_by_user_id = resolve_effective_creator("));
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    for writer in writers {
        let writer = writer.as_str().expect("writer path");
        let source = std::fs::read_to_string(root.join(writer)).expect("inventoried source");
        assert!(
            source.contains("create_in_project_with_provenance")
                || source.contains("create_in_project_with_blockers"),
            "writer bypasses provenance: {writer}"
        );
        assert!(
            !source.contains("set_created_by_user_id("),
            "post-commit attribution: {writer}"
        );
    }
}
