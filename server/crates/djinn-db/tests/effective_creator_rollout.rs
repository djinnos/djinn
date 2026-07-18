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

/// A source file is test-only only when its crate root attaches a test or
/// `test-support` cfg directly to that module declaration. This deliberately
/// inspects module structure instead of trusting a helper-like filename.
fn structurally_test_only_module(file: &Path) -> bool {
    let Some(stem) = file.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let Some(crate_root) = file.parent().map(|directory| directory.join("lib.rs")) else {
        return false;
    };
    let Ok(root) = std::fs::read_to_string(crate_root) else {
        return false;
    };
    let declaration = format!("mod {stem};");
    let Some(declaration_offset) = root.find(&declaration) else {
        return false;
    };
    let preceding = &root[..declaration_offset];
    let Some(attribute_offset) = preceding.rfind("#[cfg(") else {
        return false;
    };
    let attributes = &preceding[attribute_offset..];
    !attributes.contains(';')
        && (attributes.contains("cfg(test)") || attributes.contains("feature = \"test-support\""))
}

/// Determine whether a cfg is attached immediately to a function declaration,
/// rather than accepting an arbitrary marker elsewhere in the file.
fn function_is_test_only(source: &str, function_offset: usize) -> bool {
    let preceding = &source[..function_offset];
    let Some(attribute_offset) = preceding.rfind("#[cfg(") else {
        return false;
    };
    let attributes = &preceding[attribute_offset..];
    !attributes.contains('}')
        && (attributes.contains("cfg(test)") || attributes.contains("feature = \"test-support\""))
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
            && !structurally_test_only_module(&path)
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
        for (symbol, offset) in function_symbols(source) {
            if function_is_test_only(source, offset) {
                continue;
            }
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

/// Collect every Rust file; test code is selected syntactically below so inline
/// `#[cfg(test)]` modules in production-named files cannot be omitted.
fn rust_sources(dir: &Path, result: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("source tree readable") {
        let path = entry.expect("directory entry readable").path();
        if path.is_dir() {
            rust_sources(&path, result);
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "rs") {
            result.push(path);
        }
    }
}

fn brace_end(source: &str, open: usize) -> Option<usize> {
    let mut depth = 0_i32;
    for (relative, byte) in source[open..].bytes().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + relative + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn inline_test_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(relative) = source[from..].find("#[cfg(test)]") {
        let attribute = from + relative;
        let after_attribute = &source[attribute + "#[cfg(test)]".len()..];
        let trimmed = after_attribute.trim_start();
        let Some(after_mod) = trimmed.strip_prefix("mod ") else {
            from = attribute + "#[cfg(test)]".len();
            continue;
        };
        let module = source.len() - after_mod.len();
        let Some(open_relative) = source[module..].find('{') else {
            break;
        };
        let open = module + open_relative;
        let Some(end) = brace_end(source, open) else {
            break;
        };
        ranges.push(attribute..end);
        from = end;
    }
    ranges
}

fn external_test_source(relative: &str) -> bool {
    relative.contains("/tests/")
        || relative.ends_with("/tests.rs")
        || relative.ends_with("_tests.rs")
        || relative.ends_with("_test.rs")
        || relative.ends_with("/test_helpers.rs")
}

fn test_function_callsites(relative: &str, source: &str) -> Vec<(String, String)> {
    let inline_ranges = inline_test_ranges(source);
    let external = external_test_source(relative);
    function_symbols(source)
        .into_iter()
        .filter(|(_, offset)| external || inline_ranges.iter().any(|range| range.contains(offset)))
        .filter_map(|(symbol, _)| extract_function_body(source, &symbol).map(|body| (symbol, body)))
        .filter(|(_, body)| {
            LEGACY_CREATE_METHODS
                .iter()
                .any(|method| body.contains(method))
        })
        .collect()
}

/// Exceptions are proven by their exact enclosing function, never inferred
/// from a filename or test name. A scoped call establishes `SESSION_USER_ID`;
/// a negative resolver case names its structured unavailable-creator failure.
fn permitted_creatorless_callsite(body: &str) -> bool {
    (body.contains("SESSION_USER_ID") && body.contains(".scope("))
        || body.contains("effective_creator_unavailable")
}

fn unscoped_test_task_callsites(relative: &str, source: &str) -> Vec<String> {
    test_function_callsites(relative, source)
        .into_iter()
        .filter(|(_, body)| !permitted_creatorless_callsite(body))
        .map(|(symbol, _)| format!("{relative}::{symbol}"))
        .collect()
}

/// Every test-only convenience insertion is rejected by default. The only
/// exceptions are exact callsites whose body establishes `SESSION_USER_ID` or
/// intentionally asserts `effective_creator_unavailable`; persisted fixtures
/// must call `create_in_project_with_provenance` at insertion time.
#[test]
fn fixture_task_creation_callsites_have_creator_provenance() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../");
    let mut files = Vec::new();
    rust_sources(&root.join("server"), &mut files);
    let mut violations = Vec::new();

    for file in files {
        let source = std::fs::read_to_string(&file).expect("fixture source readable");
        let relative = file.strip_prefix(&root).unwrap().display().to_string();
        violations.extend(unscoped_test_task_callsites(&relative, &source));
    }

    assert!(
        violations.is_empty(),
        "fixture task insertion requires persisted explicit provenance; unscoped callsites: {violations:?}"
    );
}

#[test]
fn callsite_classifier_rejects_ordinary_inline_and_external_tests() {
    let call = concat!(".create_", "in_project(");
    let inline = format!(
        "\n        #[cfg(test)]\n        mod tests {{ #[test] fn ordinary_case() {{ repo{call}); }} }}\n    "
    );
    assert_eq!(
        unscoped_test_task_callsites("server/crates/example/src/ordinary.rs", &inline),
        vec!["server/crates/example/src/ordinary.rs::ordinary_case"]
    );
    let external = format!("#[test] fn ordinary_case() {{ repo{call}); }}");
    assert_eq!(
        unscoped_test_task_callsites("server/crates/example/tests/ordinary.rs", &external),
        vec!["server/crates/example/tests/ordinary.rs::ordinary_case"]
    );
}

#[test]
fn callsite_classifier_accepts_exact_scoped_and_unavailable_cases() {
    let source = r#"
        #[cfg(test)]
        mod tests {
            fn scoped() { let _scope = SESSION_USER_ID.scope(Some("real-user".into()), async {}); repo.create_in_project(); }
            fn unavailable() { let error = "effective_creator_unavailable"; repo.create_in_project(); assert!(error.contains("effective_creator_unavailable")); }
        }
    "#;
    assert!(
        unscoped_test_task_callsites("server/crates/example/src/ordinary.rs", source).is_empty()
    );
}

#[test]
fn producer_classifier_excludes_only_structurally_test_support_code() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../");
    assert!(structurally_test_only_module(
        &root.join("server/crates/djinn-agent/src/test_helpers.rs")
    ));
    assert!(structurally_test_only_module(
        &root.join("server/crates/djinn-slot/src/test_helpers.rs")
    ));

    let replay = std::fs::read_to_string(
        root.join("server/crates/djinn-slot/src/extraction_replay_eval.rs"),
    )
    .expect("replay fixture source readable");
    let (_, offset) = function_symbols(&replay)
        .into_iter()
        .find(|(symbol, _)| symbol == "run_offline_fixture_replay")
        .expect("offline replay fixture function");
    assert!(function_is_test_only(&replay, offset));

    let production = "pub async fn create_task() { repo.create_in_project_with_provenance(); }";
    let (_, offset) = function_symbols(production)
        .into_iter()
        .next()
        .expect("production function");
    assert!(!function_is_test_only(production, offset));
}
