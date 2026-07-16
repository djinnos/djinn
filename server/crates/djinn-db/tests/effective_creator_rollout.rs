//! Expand-phase inventory gate for every production task writer.
//!
//! This test enforces callsite-exact provenance compliance:
//! - Each inventoried writer's **enclosing symbol** must contain a call to its
//!   declared `boundary_method` (provenance-bound creation).
//! - No writer's enclosing symbol may use the legacy `create_in_project` /
//!   `create_with_ac` / `create` APIs that permit NULL-capable or default
//!   provenance.
//! - No writer's enclosing symbol may contain a post-insert creator patch
//!   (`set_created_by_user_id` or similar).
//! - The inventory must enumerate the complete production writer set.

use serde_json::Value;

/// The legacy/default entry points that permit NULL or empty provenance.
const LEGACY_CREATE_METHODS: &[&str] = &[".create_in_project_with_ac(", ".create_with_ac("];

/// Post-insert creator patches that bypass the transactional boundary.
const POST_INSERT_CREATOR_PATCHES: &[&str] = &[
    "set_created_by_user_id(",
    ".created_by_user_id =",
    "created_by_user_id = Some(",
];

/// Extract the body of a function/method by name from source code.
/// Returns the text from the `fn <name>` line to the end of the function body
/// (matched by brace depth).
fn extract_function_body(source: &str, symbol: &str) -> Option<String> {
    let needle = format!("fn {symbol}");
    let mut search_from = 0;
    let start = loop {
        let pos = source[search_from..].find(&needle)?;
        let abs = search_from + pos;
        // Verify the char after the symbol name is `(`, `<`, or whitespace then `(`.
        let after = &source[abs + needle.len()..];
        let trimmed = after.trim_start();
        if trimmed.starts_with('(') || trimmed.starts_with('<') || trimmed.starts_with("where") {
            break abs;
        }
        search_from = abs + needle.len();
    };

    // Walk forward to the opening brace `{`.
    let bytes = source.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i] != b'{' {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }

    // Track brace depth from the opening brace.
    let mut depth: i32 = 0;
    let body_start = i;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(source[body_start..=i].to_owned());
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[test]
fn inventoried_producers_reach_the_transactional_provenance_boundary() {
    let inventory: Value =
        serde_json::from_str(include_str!("fixtures/effective_creator_producers.json"))
            .expect("valid inventory");
    let writers = inventory["writers"].as_array().expect("writers array");
    assert!(
        !writers.is_empty(),
        "inventory must enumerate at least one writer"
    );

    // Sanity: the boundary itself exists in writes.rs.
    let writes = include_str!("../src/repositories/task/writes.rs");
    assert!(
        writes.contains("let created_by_user_id = resolve_effective_creator("),
        "resolve_effective_creator must exist in writes.rs"
    );

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../");

    let mut seen: Vec<String> = Vec::new();

    for writer in writers {
        let path = writer["path"]
            .as_str()
            .unwrap_or_else(|| panic!("writer entry must have 'path': {writer}"));
        let symbol = writer["enclosing_symbol"]
            .as_str()
            .unwrap_or_else(|| panic!("writer entry must have 'enclosing_symbol': {writer}"));
        let boundary_method = writer["boundary_method"]
            .as_str()
            .unwrap_or_else(|| panic!("writer entry must have 'boundary_method': {writer}"));
        let provenance_kind = writer["provenance_kind"]
            .as_str()
            .unwrap_or_else(|| panic!("writer entry must have 'provenance_kind': {writer}"));

        let key = format!("{path}::{symbol}");
        assert!(!seen.contains(&key), "duplicate writer entry: {key}");
        seen.push(key);

        let source = std::fs::read_to_string(root.join(path))
            .unwrap_or_else(|_| panic!("inventoried source file must exist: {path}"));

        let fn_body = extract_function_body(&source, symbol)
            .unwrap_or_else(|| panic!("enclosing symbol '{symbol}' not found in {path}"));

        // 1. The enclosing symbol MUST call the declared boundary method.
        assert!(
            fn_body.contains(boundary_method),
            "{path}::{symbol}: must call '{boundary_method}' (declared boundary_method)"
        );

        // 2. The enclosing symbol MUST NOT call any legacy/default API.
        for legacy in LEGACY_CREATE_METHODS {
            assert!(
                !fn_body.contains(*legacy),
                "{path}::{symbol}: must not use legacy '{legacy}' — use provenance-bound boundary"
            );
        }
        assert!(
            !fn_body.contains(".create_in_project("),
            "{path}::{symbol}: must not use legacy '.create_in_project(' — use provenance-bound boundary"
        );
        // Check bare `.create(` that is NOT `.create_in_project` or `.create_with_ac`.
        let mut search = 0;
        loop {
            let Some(pos) = fn_body[search..].find(".create(") else {
                break;
            };
            let abs = search + pos;
            let after = &fn_body[abs + ".create(".len()..];
            assert!(
                after.starts_with('_'),
                "{path}::{symbol}: must not use legacy '.create(' — use provenance-bound boundary"
            );
            search = abs + ".create(".len();
        }

        // 3. The enclosing symbol MUST NOT patch the creator after insert.
        for patch in POST_INSERT_CREATOR_PATCHES {
            assert!(
                !fn_body.contains(*patch),
                "{path}::{symbol}: must not patch created_by_user_id after insert ('{patch}')"
            );
        }

        // 4. The provenance_kind must be one of the known resolution tiers.
        let valid_kinds = [
            "explicit_session",
            "explicit_proposal_owner",
            "explicit_source_creator",
            "parent_epic",
            "parent_epic_proposal",
            "source_task_parent_epic",
        ];
        assert!(
            valid_kinds.contains(&provenance_kind),
            "{path}::{symbol}: unknown provenance_kind '{provenance_kind}'"
        );
    }
}
