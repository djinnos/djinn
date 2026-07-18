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

fn directly_attached_attributes(source: &str, item_offset: usize) -> Vec<&str> {
    let mut preceding = source[..item_offset].trim_end();
    let mut attributes = Vec::new();
    while preceding.ends_with(']') {
        let Some(attribute_offset) = preceding.rfind("#[") else {
            break;
        };
        let attribute = &preceding[attribute_offset..];
        if attribute.contains(';') || attribute.contains('{') || attribute.contains('}') {
            break;
        }
        attributes.push(attribute);
        preceding = preceding[..attribute_offset].trim_end();
    }
    attributes
}

fn attributes_are_test_only(attributes: &[&str]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.starts_with("#[cfg(")
            && (attribute.contains("cfg(test)")
                || attribute.contains("any(test,")
                || attribute.contains("feature = \"test-support\""))
    })
}

/// A source file is test-only only when its crate root attaches a test or
/// `test-support` cfg directly to that module declaration. This deliberately
/// inspects module structure instead of trusting a helper-like filename.
fn structurally_test_only_module(file: &Path) -> bool {
    structurally_test_only_module_inner(file, &mut BTreeSet::new())
        || structurally_exported_test_support(file)
}

fn structurally_exported_test_support(file: &Path) -> bool {
    let Some(parent) = file.parent() else {
        return false;
    };
    let Ok(module_root) = std::fs::read_to_string(parent.join("mod.rs")) else {
        return false;
    };
    let Some(src) = parent.parent() else {
        return false;
    };
    let Ok(crate_root) = std::fs::read_to_string(src.join("lib.rs")) else {
        return false;
    };
    module_root.contains("pub mod test_support;")
        && crate_root.contains("pub mod test_support {")
        && crate_root.contains("pub use crate::repositories::test_support::")
        && file
            .file_name()
            .is_some_and(|name| name == "test_support.rs")
}

fn structurally_test_only_module_inner(file: &Path, visited: &mut BTreeSet<PathBuf>) -> bool {
    if !visited.insert(file.to_path_buf()) {
        return false;
    }
    let Some(stem) = file.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let Some(parent) = file.parent() else {
        return false;
    };
    let mut owners = vec![parent.join("mod.rs"), parent.join("lib.rs")];
    if let (Some(directory_name), Some(grandparent)) = (
        parent.file_name().and_then(|name| name.to_str()),
        parent.parent(),
    ) {
        owners.push(grandparent.join(format!("{directory_name}.rs")));
    }
    if let Ok(entries) = std::fs::read_dir(parent) {
        owners.extend(entries.filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension().is_some_and(|extension| extension == "rs")).then_some(path)
        }));
    }
    owners.sort();
    owners.dedup();

    let file_name = file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    for owner in owners.into_iter().filter(|owner| owner != file) {
        let Ok(source) = std::fs::read_to_string(&owner) else {
            continue;
        };
        let normal_declaration = format!("mod {stem};");
        let path_attribute = format!("#[path = \"{file_name}\"]");
        let include = format!("include!(\"{file_name}\")");
        if let Some(include_offset) = source.find(&include) {
            let item_offset = source[..include_offset]
                .rfind('\n')
                .map_or(0, |line_end| line_end + 1);
            if attributes_are_test_only(&directly_attached_attributes(&source, item_offset))
                || inline_test_ranges(&source)
                    .iter()
                    .any(|module| module.contains(&include_offset))
            {
                return true;
            }
        }
        let declaration_offset = source
            .find(&path_attribute)
            .and_then(|path_offset| {
                source[path_offset..]
                    .find("mod ")
                    .map(|relative| path_offset + relative)
            })
            .or_else(|| source.find(&normal_declaration));
        let Some(declaration_offset) = declaration_offset else {
            continue;
        };
        let item_offset = source[..declaration_offset]
            .rfind('\n')
            .map_or(0, |line_end| line_end + 1);
        if attributes_are_test_only(&directly_attached_attributes(&source, item_offset))
            || structurally_test_only_module_inner(&owner, visited)
        {
            return true;
        }
    }
    false
}

/// Determine whether a cfg is attached immediately to a function declaration,
/// rather than accepting an arbitrary marker elsewhere in the file.
fn function_is_test_only(source: &str, function_offset: usize) -> bool {
    attributes_are_test_only(&directly_attached_attributes(source, function_offset))
}

fn appended_test_module_start(source: &str) -> Option<usize> {
    let module = source.rfind("\nmod tests {")? + 1;
    attributes_are_test_only(&directly_attached_attributes(source, module)).then_some(module)
}

fn production_source_path(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "rs")
        && path.to_string_lossy().contains("/src/")
}

fn production_sources(dir: &Path, root: &Path, result: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("source tree readable") {
        let path = entry.expect("directory entry readable").path();
        if path.is_dir() {
            production_sources(&path, root, result);
            continue;
        }
        let relative = path.strip_prefix(root).expect("under repository root");
        if production_source_path(relative) && !structurally_test_only_module(&path) {
            result.push(path);
        }
    }
}

/// Discover production writers from the source tree rather than trusting the fixture.
/// Only modules and functions proven test-only by an attached cfg are excluded.
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
        let inline_test_modules = inline_test_ranges(&source);
        let appended_test_module = appended_test_module_start(&source);
        let direct_constants = direct_task_insert_constants(&source);
        for (symbol, offset) in function_symbols(&source) {
            if function_is_test_only(&source, offset)
                || appended_test_module.is_some_and(|module| offset >= module)
                || inline_test_modules
                    .iter()
                    .any(|module| module.contains(&offset))
            {
                continue;
            }
            let Some(body) = extract_function_body(&source, &symbol) else {
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
    let mut offset = 0;
    for line in source.lines() {
        let item_offset = offset;
        offset += line.len() + 1;
        if !line.trim_start().starts_with("mod ")
            || !attributes_are_test_only(&directly_attached_attributes(source, item_offset))
        {
            continue;
        }
        let module = item_offset + line.find("mod ").expect("module line");
        let Some(open_relative) = source[module..].find('{') else {
            continue;
        };
        let open = module + open_relative;
        if source[module..open].contains(';') {
            continue;
        }
        let Some(end) = brace_end(source, open) else {
            continue;
        };
        ranges.push(item_offset..end);
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

    let intervening_declaration = r#"
        #[cfg(test)]
        const FIXTURE: &str = "marker";
        pub async fn runtime_writer() { repo.create_in_project_with_provenance(); }
    "#;
    let (_, offset) = function_symbols(intervening_declaration)
        .into_iter()
        .find(|(symbol, _)| symbol == "runtime_writer")
        .expect("runtime writer");
    assert!(!function_is_test_only(intervening_declaration, offset));

    let intervening_marker = r#"
        #[cfg(test)]
        // This marker is not an attribute on the runtime writer.
        const MARKER: () = ();
        pub async fn runtime_writer() { repo.create_in_project_with_provenance(); }
    "#;
    let (_, offset) = function_symbols(intervening_marker)
        .into_iter()
        .find(|(symbol, _)| symbol == "runtime_writer")
        .expect("runtime writer after marker");
    assert!(!function_is_test_only(intervening_marker, offset));

    for helper_like_path in [
        "server/crates/example/src/runtime_test_support.rs",
        "server/crates/example/src/runtime_tests.rs",
        "server/crates/example/src/tests/runtime_writer.rs",
    ] {
        assert!(
            production_source_path(Path::new(helper_like_path)),
            "helper-like path must not suppress runtime writers: {helper_like_path}"
        );
    }
}
