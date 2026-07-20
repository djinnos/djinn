//! Expand-phase inventory gate for every production task writer.
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
#[path = "migrations_task_creator_contract.rs"]
mod migrations_task_creator_contract;

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

struct CfgExpression<'a> {
    remaining: &'a str,
}

impl<'a> CfgExpression<'a> {
    fn new(expression: &'a str) -> Self {
        Self {
            remaining: expression,
        }
    }

    fn skip_whitespace(&mut self) {
        self.remaining = self.remaining.trim_start();
    }

    fn consume(&mut self, expected: char) -> bool {
        self.skip_whitespace();
        if self.remaining.starts_with(expected) {
            self.remaining = &self.remaining[expected.len_utf8()..];
            true
        } else {
            false
        }
    }

    fn identifier(&mut self) -> Option<&'a str> {
        self.skip_whitespace();
        let end = self
            .remaining
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(self.remaining.len());
        if end == 0 {
            return None;
        }
        let identifier = &self.remaining[..end];
        self.remaining = &self.remaining[end..];
        Some(identifier)
    }

    fn quoted_value(&mut self) -> Option<&'a str> {
        self.skip_whitespace();
        let value = self.remaining.strip_prefix('"')?;
        let end = value.find('"')?;
        self.remaining = &value[end + 1..];
        Some(&value[..end])
    }

    /// Return whether every configuration satisfying this expression must
    /// enable either `test` or the `test-support` feature.
    fn necessarily_test_only(&mut self) -> Option<bool> {
        let predicate = self.identifier()?;
        self.skip_whitespace();

        if self.consume('=') {
            let value = self.quoted_value()?;
            return Some(predicate == "feature" && value == "test-support");
        }
        if !self.consume('(') {
            return Some(predicate == "test");
        }

        let mut operands = Vec::new();
        loop {
            self.skip_whitespace();
            if self.consume(')') {
                break;
            }
            operands.push(self.necessarily_test_only()?);
            self.skip_whitespace();
            if self.consume(')') {
                break;
            }
            if !self.consume(',') {
                return None;
            }
        }

        match predicate {
            // A conjunction is test-only if at least one mandatory operand is.
            "all" => Some(!operands.is_empty() && operands.into_iter().any(|value| value)),
            // Every disjunct must independently require a test-only setting.
            "any" => Some(!operands.is_empty() && operands.into_iter().all(|value| value)),
            // Negation and unknown cfg operators are conservatively production-capable.
            _ => Some(false),
        }
    }
}

fn attributes_are_test_only(attributes: &[&str]) -> bool {
    attributes.iter().any(|attribute| {
        let Some(expression) = attribute
            .strip_prefix("#[cfg(")
            .and_then(|attribute| attribute.strip_suffix(")]"))
        else {
            return false;
        };
        let mut expression = CfgExpression::new(expression);
        expression.necessarily_test_only() == Some(true) && expression.remaining.trim().is_empty()
    })
}

/// A source file is test-only only when its crate root attaches a test or
/// `test-support` cfg directly to that module declaration. This deliberately
/// inspects module structure instead of trusting a helper-like filename.
fn structurally_test_only_module(file: &Path) -> bool {
    structurally_test_only_module_inner(file, &mut BTreeSet::new())
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

fn production_writers_in_source(path: &str, source: &str) -> BTreeSet<String> {
    let mut discovered = BTreeSet::new();
    let is_db_repository = path.contains("djinn-db/src/repositories/");
    let is_shared_task_boundary = path.ends_with("repositories/task/writes.rs")
        || path.ends_with("repositories/task/reads.rs");
    let inline_test_modules = inline_test_ranges(source);
    let direct_constants = direct_task_insert_constants(source);
    for (symbol, offset) in function_symbols(source) {
        if function_is_test_only(source, offset)
            || inline_test_modules
                .iter()
                .any(|module| module.contains(&offset))
        {
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
    discovered
}

/// Discover production writers from the source tree rather than trusting the fixture.
/// Only modules and functions proven test-only by an attached cfg are excluded.
fn discover_production_writers(root: &Path) -> BTreeSet<String> {
    let mut files = Vec::new();
    production_sources(&root.join("server/crates"), root, &mut files);
    let mut discovered = BTreeSet::new();
    for file in files {
        let source = std::fs::read_to_string(&file).expect("production source readable");
        let path = file.strip_prefix(root).unwrap().to_string_lossy();
        discovered.extend(production_writers_in_source(&path, &source));
    }
    discovered
}

fn manifest_section<'a>(manifest: &'a str, heading: &str) -> &'a str {
    let heading = format!("[{heading}]");
    let section = manifest
        .split_once(&heading)
        .unwrap_or_else(|| panic!("manifest section {heading} must exist"))
        .1;
    section.find("\n[").map_or(section, |next| &section[..next])
}

#[test]
fn coordinator_enables_db_test_support_only_on_its_dev_edge() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../");
    let manifest = std::fs::read_to_string(root.join("server/crates/djinn-coordinator/Cargo.toml"))
        .expect("coordinator manifest readable");
    let normal = manifest_section(&manifest, "dependencies");
    let dev = manifest_section(&manifest, "dev-dependencies");
    assert!(
        normal
            .lines()
            .any(|line| line.trim() == r#"djinn-db = { path = "../djinn-db" }"#)
    );
    assert!(!normal.contains("test-support"));
    assert!(
        dev.lines().any(|line| line.trim()
            == r#"djinn-db = { path = "../djinn-db", features = ["test-support"] }"#)
    );
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
    let bytes = source.as_bytes();
    let mut depth = 0_i32;
    let mut offset = open;
    while offset < bytes.len() {
        match bytes[offset] {
            b'/' if bytes.get(offset + 1) == Some(&b'/') => {
                offset = source[offset..]
                    .find('\n')
                    .map_or(bytes.len(), |end| offset + end + 1);
                continue;
            }
            b'/' if bytes.get(offset + 1) == Some(&b'*') => {
                let mut comment_depth = 1_u32;
                offset += 2;
                while offset < bytes.len() && comment_depth > 0 {
                    if bytes[offset..].starts_with(b"/*") {
                        comment_depth += 1;
                        offset += 2;
                    } else if bytes[offset..].starts_with(b"*/") {
                        comment_depth -= 1;
                        offset += 2;
                    } else {
                        offset += 1;
                    }
                }
                continue;
            }
            b'"' => {
                offset += 1;
                while offset < bytes.len() {
                    match bytes[offset] {
                        b'\\' => offset += 2,
                        b'"' => {
                            offset += 1;
                            break;
                        }
                        _ => offset += 1,
                    }
                }
                continue;
            }
            b'r' | b'b' => {
                let prefix = offset;
                let mut marker = offset + usize::from(bytes[offset] == b'b');
                if bytes.get(marker) == Some(&b'r') {
                    marker += 1;
                } else if bytes[offset] == b'b' && bytes.get(marker) == Some(&b'"') {
                    offset = marker;
                    continue;
                } else {
                    offset += 1;
                    continue;
                }
                let hashes = bytes[marker..]
                    .iter()
                    .take_while(|byte| **byte == b'#')
                    .count();
                marker += hashes;
                if bytes.get(marker) != Some(&b'"') {
                    offset = prefix + 1;
                    continue;
                }
                let terminator = format!("\"{}", "#".repeat(hashes));
                offset = source[marker + 1..]
                    .find(&terminator)
                    .map_or(bytes.len(), |end| marker + 1 + end + terminator.len());
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset + 1);
                }
            }
            _ => {}
        }
        offset += 1;
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

    for production_capable_cfg in [
        r#"#[cfg(not(feature = "test-support"))]
pub async fn runtime_writer() { repo.create_in_project_with_provenance(); }"#,
        r#"#[cfg(any(unix, feature = "test-support"))]
pub async fn runtime_writer() { repo.create_in_project_with_provenance(); }"#,
    ] {
        let (_, offset) = function_symbols(production_capable_cfg)
            .into_iter()
            .next()
            .expect("cfg-gated runtime writer");
        assert!(!function_is_test_only(production_capable_cfg, offset));
    }

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

    let ungated_public_test_support =
        "pub async fn runtime_writer() { repo.create_in_project_with_provenance(); }";
    assert_eq!(
        production_writers_in_source(
            "server/crates/example/src/test_support.rs",
            ungated_public_test_support,
        ),
        BTreeSet::from([String::from(
            "server/crates/example/src/test_support.rs::runtime_writer"
        )]),
        "an ungated publicly exported test_support module remains production-capable"
    );

    let trailing_runtime_writer = r#"
        #[cfg(test)]
        mod tests {
            fn fixture_writer() { repo.create_in_project_with_provenance(); }
        }
        pub async fn runtime_writer() { repo.create_in_project_with_provenance(); }
    "#;
    assert_eq!(
        production_writers_in_source(
            "server/crates/example/src/runtime.rs",
            trailing_runtime_writer
        ),
        BTreeSet::from([String::from(
            "server/crates/example/src/runtime.rs::runtime_writer"
        )]),
        "a writer after a bounded cfg(test) module remains a production candidate"
    );
}

#[test]
fn release_creator_contract_is_concrete_and_legacy_guards_remain_intentional() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../");
    let task_model =
        std::fs::read_to_string(root.join("server/crates/djinn-core/src/models/task.rs"))
            .expect("Task model readable");
    let task_field = task_model
        .split_once("pub created_by_user_id:")
        .expect("Task creator field");
    assert!(
        task_field.1.starts_with(" String,"),
        "Task creator must be concrete String"
    );
    assert!(
        !task_field.0.ends_with("sqlx(default))]\n    "),
        "Task creator must not silently decode a missing SQL column"
    );
    let writes = std::fs::read_to_string(
        root.join("server/crates/djinn-db/src/repositories/task/writes.rs"),
    )
    .expect("task writer readable");
    assert!(!writes.contains("clear_created_by_user_id"));
    assert!(writes.contains("EFFECTIVE_CREATOR_UNAVAILABLE"));
    let board_health = std::fs::read_to_string(
        root.join("server/crates/djinn-db/src/repositories/task/board_health.rs"),
    )
    .expect("board-health source readable");
    assert!(
        board_health.contains("t.created_by_user_id IS NULL"),
        "preserve legacy guard until 3qi0"
    );
    let recovery = std::fs::read_to_string(
        root.join("server/crates/djinn-coordinator/src/refinement_recovery.rs"),
    )
    .expect("refinement recovery source readable");
    assert!(recovery.contains("proposal.refinement_owner_user_id.clone()"));
}
