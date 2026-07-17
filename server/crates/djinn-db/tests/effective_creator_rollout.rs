//! Expand-phase inventory gate for every production task writer.
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const LEGACY_CREATE_METHODS: &[&str] = &[
    ".create_in_project_with_ac(",
    ".create_with_ac(",
    ".create_in_project(",
];
const POST_INSERT_CREATOR_PATCHES: &[&str] = &[
    "set_created_by_user_id(",
    ".created_by_user_id =",
    "created_by_user_id = Some(",
];
const BOUNDARIES: &[&str] = &[
    "create_in_project_with_provenance(",
    "create_in_project_with_blockers(",
];

fn extract_function_body(source: &str, symbol: &str) -> Option<String> {
    let needle = format!("fn {symbol}");
    let mut search_from = 0;
    let start = loop {
        let pos = source[search_from..].find(&needle)?;
        let abs = search_from + pos;
        let trimmed = source[abs + needle.len()..].trim_start();
        if trimmed.starts_with('(') || trimmed.starts_with('<') || trimmed.starts_with("where") {
            break abs;
        }
        search_from = abs + needle.len();
    };
    let bytes = source.as_bytes();
    let mut i = start;
    while i < bytes.len() && bytes[i] != b'{' {
        i += 1;
    }
    if i == bytes.len() {
        return None;
    }
    let body_start = i;
    let mut depth = 0_i32;
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

fn function_symbols(source: &str) -> Vec<(String, usize)> {
    source
        .lines()
        .scan(0, |offset, line| {
            let line_offset = *offset;
            *offset += line.len() + 1;
            Some((line, line_offset))
        })
        .filter_map(|(line, offset)| {
            let before = line.split("fn ").next()?;
            if before.contains("//") {
                return None;
            }
            let tail = line.split_once("fn ")?.1;
            let name: String = tail
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            (!name.is_empty()).then_some((name, offset))
        })
        .collect()
}

fn contains_task_insert(source: &str) -> bool {
    source
        .split_whitespace()
        .collect::<String>()
        .to_ascii_uppercase()
        .contains("INSERTINTOTASKS")
}

/// Discover named SQL statements generically, rather than recognizing one
/// audit-specific constant name.
fn direct_task_insert_constants(source: &str) -> Vec<(String, String)> {
    let mut constants = Vec::new();
    let mut search_from = 0;
    while let Some(relative) = source[search_from..].find("const ") {
        let start = search_from + relative;
        let tail = &source[start + "const ".len()..];
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let Some(relative_end) = tail.find(";\n") else {
            break;
        };
        let declaration = tail[..relative_end + 1].to_owned();
        if !name.is_empty() && contains_task_insert(&declaration) {
            constants.push((name, declaration));
        }
        search_from = start + "const ".len() + relative_end + 2;
    }
    constants
}

fn production_source(source: &str) -> &str {
    let Some(test_module) = source.rfind("\nmod tests {") else {
        return source;
    };
    let prefix = &source[..test_module];
    prefix
        .rfind("#[cfg(test)]")
        .map_or(source, |attribute| &source[..attribute])
}

fn production_sources(dir: &Path, root: &Path, result: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("source tree readable") {
        let path = entry.expect("directory entry readable").path();
        if path.is_dir() {
            production_sources(&path, root, result);
            continue;
        }
        let relative = path.strip_prefix(root).expect("under repository root");
        let text = relative.to_string_lossy();
        if path.extension().is_some_and(|ext| ext == "rs")
            && text.contains("/src/")
            && !text.contains("/tests/")
            && !text.ends_with("/tests.rs")
            && !text.ends_with("_tests.rs")
            && !text.contains("test_support")
        {
            result.push(path);
        }
    }
}

/// Discover production writers from the source tree rather than trusting the fixture.
/// Test modules, integration-test trees, and fixture helpers are deliberately excluded.
fn discover_production_writers(root: &Path) -> BTreeSet<String> {
    let mut files = Vec::new();
    production_sources(&root.join("server/crates"), root, &mut files);
    let mut discovered = BTreeSet::new();
    for file in files {
        let source = std::fs::read_to_string(&file).expect("production source readable");
        let path = file
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let is_db_repository = path.contains("djinn-db/src/repositories/");
        let is_shared_task_boundary = path.ends_with("repositories/task/writes.rs")
            || path.ends_with("repositories/task/reads.rs");
        // Test modules are conventionally appended to a repository file. Their
        // fixture SQL is intentionally outside the production discovery set;
        // inline `#[cfg(test)]` hooks in production methods remain visible.
        let source = production_source(&source);
        let direct_constants = direct_task_insert_constants(source);
        for (symbol, _) in function_symbols(source) {
            let Some(body) = extract_function_body(source, &symbol) else {
                continue;
            };
            let direct_insert = contains_task_insert(&body)
                || direct_constants
                    .iter()
                    .any(|(constant, _)| body.contains(constant));
            if (!is_db_repository && BOUNDARIES.iter().any(|boundary| body.contains(boundary)))
                || (direct_insert && !is_shared_task_boundary)
            {
                discovered.insert(format!("{path}::{symbol}"));
            }
        }
    }
    discovered
}

#[test]
fn inventoried_producers_reach_the_transactional_provenance_boundary() {
    let inventory: Value =
        serde_json::from_str(include_str!("fixtures/effective_creator_producers.json"))
            .expect("valid inventory");
    let writers = inventory["writers"].as_array().expect("writers array");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../");
    let mut inventory_keys = BTreeSet::new();

    for writer in writers {
        let path = writer["path"].as_str().expect("writer path");
        let symbol = writer["enclosing_symbol"].as_str().expect("writer symbol");
        let boundary = writer["boundary_method"].as_str().expect("writer boundary");
        let kind = writer["provenance_kind"]
            .as_str()
            .expect("writer provenance");
        let key = format!("{path}::{symbol}");
        assert!(
            inventory_keys.insert(key.clone()),
            "duplicate writer entry: {key}"
        );
        let source = std::fs::read_to_string(root.join(path)).expect("inventoried source exists");
        let body = extract_function_body(&source, symbol)
            .unwrap_or_else(|| panic!("symbol not found: {key}"));
        assert!(
            body.contains(boundary),
            "{key}: must call declared boundary {boundary}"
        );
        for legacy in LEGACY_CREATE_METHODS {
            assert!(!body.contains(legacy), "{key}: legacy writer API {legacy}");
        }
        for patch in POST_INSERT_CREATOR_PATCHES {
            assert!(
                !body.contains(patch),
                "{key}: post-insert creator patch {patch}"
            );
        }
        assert!(
            [
                "explicit_session",
                "explicit_proposal_owner",
                "explicit_source_creator",
                "parent_epic",
                "parent_epic_proposal",
                "source_task_parent_epic"
            ]
            .contains(&kind),
            "{key}: unknown provenance kind"
        );

        // Direct SQL writers must visibly insert the resolved concrete creator,
        // not merely invoke a resolver before a NULL-capable statement.
        let direct_inserts: Vec<_> = direct_task_insert_constants(&source)
            .into_iter()
            .filter(|(constant, _)| body.contains(constant))
            .collect();
        if !direct_inserts.is_empty() || contains_task_insert(&body) {
            let insert_sql = direct_inserts
                .iter()
                .map(|(_, sql)| sql.as_str())
                .collect::<String>();
            assert!(
                insert_sql.contains("created_by_user_id") || body.contains("created_by_user_id"),
                "{key}: INSERT must name created_by_user_id"
            );
            assert!(
                !insert_sql.to_ascii_uppercase().contains("NULL"),
                "{key}: INSERT must bind concrete creator, not NULL"
            );
            assert!(
                body.contains(".bind(&created_by_user_id)"),
                "{key}: INSERT must bind resolved creator"
            );
        }
    }

    let discovered = discover_production_writers(&root);
    assert_eq!(
        discovered, inventory_keys,
        "fixture must exactly cover every discovered production task writer"
    );
}

/// Recursively collect Rust test and shared-test-helper sources.
fn fixture_test_sources(dir: &Path, root: &Path, result: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("source tree readable") {
        let path = entry.expect("directory entry readable").path();
        if path.is_dir() {
            fixture_test_sources(&path, root, result);
            continue;
        }
        let relative = path.strip_prefix(root).expect("under repository root");
        let text = relative.to_string_lossy();
        if path.extension().is_some_and(|ext| ext == "rs")
            && (text.contains("/tests/")
                || text.ends_with("_tests.rs")
                || text.ends_with("/tests.rs")
                || text.ends_with("/test_helpers.rs"))
        {
            result.push(path);
        }
    }
}

/// Fixture builders and fixture-bearing direct tests must not use the
/// creator-less convenience insertion API. The only permitted uses are
/// deliberately session-scoped tests and intentional
/// `effective_creator_unavailable` resolver tests; both are recognized from
/// the enclosing callsite body, not from a file allowlist.
#[test]
fn fixture_task_creation_callsites_have_creator_provenance() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../");
    let mut files = Vec::new();
    fixture_test_sources(&root.join("server"), &root, &mut files);
    let mut violations = Vec::new();

    for file in files {
        let source = std::fs::read_to_string(&file).expect("fixture source readable");
        let relative = file.strip_prefix(&root).unwrap().display().to_string();
        for (symbol, _) in function_symbols(&source) {
            let Some(body) = extract_function_body(&source, &symbol) else {
                continue;
            };
            if !body.contains(".create_in_project(") {
                continue;
            }
            let fixture_boundary = relative.ends_with("/test_helpers.rs")
                || symbol.contains("fixture")
                || symbol.contains("replay")
                || symbol.contains("seed_");
            if !fixture_boundary {
                continue;
            }
            let deliberately_scoped = body.contains("SESSION_USER_ID") && body.contains(".scope(");
            let intentional_negative = body.contains("effective_creator_unavailable");
            if !deliberately_scoped && !intentional_negative {
                violations.push(format!("{relative}::{symbol}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "fixture task insertion requires persisted explicit provenance; unscoped callsites: {violations:?}"
    );
}
