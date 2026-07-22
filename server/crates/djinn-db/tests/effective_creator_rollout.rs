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
    // Only the boundary implementation file itself is exempt from direct-insert
    // discovery; the peer-sync writers in `task/reads.rs` are inventoried
    // producers and must be discovered like any other production writer.
    let is_shared_task_boundary = path.ends_with("repositories/task/writes.rs");
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

// ═══════════════════════════════════════════════════════════════════════════
// Schema-graduation proof surface: invoke the migration-140 contract matrix
// ═══════════════════════════════════════════════════════════════════════════
//
// The migration was renumbered from 140 to 142 after main claimed 141 (sibling
// `typx`). The rollout gate reuses the same shared support module that the
// dedicated `migrations_task_creator_contract` test binary uses, so the
// migration matrix is invoked — not duplicated — as part of the single
// executable proof surface.

#[path = "support/migrations_task_creator_contract.rs"]
#[allow(dead_code)]
mod contract_support;

use djinn_db::migrations::{self, DesignatedOperatorBootstrap, MigrationContext};
use sqlx::postgres::PgConnection;
use sqlx::{Connection, Executor};

/// The migration file under test — renamed from 140 to 142 by `typx`.
#[test]
fn migration_matrix_file_is_the_renumbered_creator_contract() {
    let dir = contract_support::migrations_dir();
    let path = dir.join(contract_support::MIGRATION_FILE);
    assert!(
        path.exists(),
        "creator-contract migration file must exist at {}",
        path.display()
    );
    assert_eq!(contract_support::MIGRATION_VERSION, 142);
}

/// Preflight ordering: an unset designated operator must abort before any
/// write, leaving the column nullable and committing no migration row.
#[tokio::test]
async fn matrix_preflight_unset_operator_aborts_before_writes() {
    contract_support::with_temp_database("rollout_preflight_unset", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url).await.expect("connect");
        contract_support::apply_prior_migrations(&mut conn).await;
        contract_support::clear_operator(&mut conn).await;

        let sql = contract_support::migration_sql();
        let err = conn.execute(sql.as_str()).await.expect_err("must fail");
        assert!(
            err.to_string()
                .contains("creator_contract_designated_operator_unset")
        );
        assert!(
            contract_support::column_is_nullable(&mut conn).await,
            "column must remain nullable after preflight failure"
        );
        conn.close().await.expect("close");
    })
    .await;
}

/// Deterministic precedence: source-task creator wins over epic/proposal/
/// designated; a creator-less chain lands on the designated operator (residue).
#[tokio::test]
async fn matrix_precedence_and_residue_are_deterministic() {
    contract_support::with_temp_database("rollout_precedence", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url).await.expect("connect");
        contract_support::apply_prior_migrations(&mut conn).await;

        contract_support::seed_project(&mut conn, "project-1").await;
        contract_support::seed_user(&mut conn, "u-src", false).await;
        contract_support::seed_user(&mut conn, "u-epic", false).await;
        contract_support::seed_user(&mut conn, contract_support::DESIGNATED, false).await;

        contract_support::seed_task(&mut conn, "t-src", "project-1", Some("u-src")).await;
        contract_support::seed_epic(&mut conn, "e1", "project-1", Some("u-epic")).await;
        contract_support::seed_task_with_epic(
            &mut conn,
            "t-target",
            "project-1",
            Some("e1"),
            None,
            None,
        )
        .await;
        contract_support::seed_audit_source_link(&mut conn, "t-target", "t-src", "1").await;

        contract_support::set_operator(&mut conn, contract_support::DESIGNATED).await;
        contract_support::apply_contract_migration(&mut conn).await;

        assert_eq!(
            contract_support::get_task_creator(&mut conn, "t-target").await,
            Some("u-src".to_owned()),
            "source-task creator must win over epic and designated"
        );
        assert_eq!(
            contract_support::get_task_creator(&mut conn, "t-src").await,
            Some("u-src".to_owned()),
            "existing non-NULL creator must be preserved"
        );
        conn.close().await.expect("close");
    })
    .await;
}

/// Rollback: a forced failure between the data step and the schema contraction
/// must restore both the data and the nullable column.
#[tokio::test]
async fn matrix_rollback_restores_data_and_schema() {
    contract_support::with_temp_database("rollout_rollback", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url).await.expect("connect");
        contract_support::apply_prior_migrations(&mut conn).await;

        contract_support::seed_project(&mut conn, "p1").await;
        contract_support::seed_user(&mut conn, contract_support::DESIGNATED, false).await;
        contract_support::seed_task(&mut conn, "t-rb", "p1", None).await;

        contract_support::set_operator(&mut conn, contract_support::DESIGNATED).await;

        let full_sql = contract_support::migration_sql();
        let data_step = contract_support::migration_data_step_sql();
        let preflight_end = full_sql
            .find("WITH valid_source AS (")
            .expect("find data step start");
        let preflight_sql = full_sql[..preflight_end].trim();

        let mut tx = conn.begin().await.expect("begin");
        tx.execute(preflight_sql).await.expect("preflight");
        tx.execute(data_step.as_str()).await.expect("data step");
        let updated: Option<String> =
            sqlx::query_scalar("SELECT created_by_user_id FROM tasks WHERE id = 't-rb'")
                .fetch_one(&mut *tx)
                .await
                .expect("check");
        assert_eq!(updated, Some(contract_support::DESIGNATED.to_owned()));
        let forced = tx.execute("SELECT 1 / 0").await;
        assert!(forced.is_err(), "forced failure");
        drop(tx);

        assert_eq!(
            contract_support::get_task_creator(&mut conn, "t-rb").await,
            None,
            "data must be restored to NULL after rollback"
        );
        assert!(
            contract_support::column_is_nullable(&mut conn).await,
            "column must remain nullable after rollback"
        );
        conn.close().await.expect("close");
    })
    .await;
}

/// Idempotence: rerunning the real data step after a successful migration
/// must affect zero rows and leave creators unchanged.
#[tokio::test]
async fn matrix_data_step_is_idempotent() {
    contract_support::with_temp_database("rollout_idem", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url).await.expect("connect");
        contract_support::apply_prior_migrations(&mut conn).await;

        contract_support::seed_project(&mut conn, "p1").await;
        contract_support::seed_user(&mut conn, contract_support::DESIGNATED, false).await;
        contract_support::seed_task(&mut conn, "t1", "p1", None).await;

        contract_support::set_operator(&mut conn, contract_support::DESIGNATED).await;
        contract_support::apply_contract_migration(&mut conn).await;

        let creator_before = contract_support::get_task_creator(&mut conn, "t1").await;
        assert_eq!(
            creator_before,
            Some(contract_support::DESIGNATED.to_owned())
        );

        let data_step = contract_support::migration_data_step_sql();
        let affected: i64 = conn
            .execute(data_step.as_str())
            .await
            .map(|r| r.rows_affected() as i64)
            .expect("rerun");
        assert_eq!(affected, 0, "idempotent rerun must affect zero rows");
        assert_eq!(
            contract_support::get_task_creator(&mut conn, "t1").await,
            creator_before,
            "creator must be unchanged after idempotent rerun"
        );
        conn.close().await.expect("close");
    })
    .await;
}

/// Zero-NULL assertion ordering, catalog non-nullability, and direct SQL NULL
/// rejection — all in one end-to-end migration run.
#[tokio::test]
async fn matrix_zero_null_ordering_catalog_and_null_rejection() {
    contract_support::with_temp_database("rollout_null_order", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url).await.expect("connect");
        contract_support::apply_prior_migrations(&mut conn).await;

        contract_support::seed_project(&mut conn, "p1").await;
        contract_support::seed_user(&mut conn, contract_support::DESIGNATED, false).await;
        contract_support::seed_task(&mut conn, "t1", "p1", None).await;

        contract_support::set_operator(&mut conn, contract_support::DESIGNATED).await;
        contract_support::apply_contract_migration(&mut conn).await;

        assert!(
            !contract_support::column_is_nullable(&mut conn).await,
            "column must be NOT NULL after migration"
        );

        let is_nullable: String = sqlx::query_scalar(
            "SELECT is_nullable FROM information_schema.columns \
             WHERE table_name = 'tasks' AND column_name = 'created_by_user_id'",
        )
        .fetch_one(&mut conn)
        .await
        .expect("catalog");
        assert_eq!(is_nullable, "NO", "catalog must report NOT NULL");

        let insert_err = sqlx::query(
            "INSERT INTO tasks \
             (id, project_id, short_id, title, description, design, \
              labels, acceptance_criteria, memory_refs, created_by_user_id) \
             VALUES ('t-null', 'p1', 'sn', 't', 'd', 'dd', \
                     '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, NULL)",
        )
        .execute(&mut conn)
        .await;
        assert!(insert_err.is_err(), "NULL INSERT must fail under NOT NULL");

        let update_err = sqlx::query("UPDATE tasks SET created_by_user_id = NULL WHERE id = 't1'")
            .execute(&mut conn)
            .await;
        assert!(update_err.is_err(), "NULL UPDATE must fail under NOT NULL");
        conn.close().await.expect("close");
    })
    .await;
}

/// The full repository-owned migration runner (the production path) must
/// apply the contract migration end-to-end with a bootstrapped operator.
#[tokio::test]
async fn matrix_full_runner_applies_contract_migration() {
    contract_support::with_temp_database("rollout_full", |db_url| async move {
        migrations::bootstrap_designated_operator(
            &db_url,
            &DesignatedOperatorBootstrap {
                user_id: contract_support::DESIGNATED.to_owned(),
                github_id: 9_000_000_099,
                github_login: "rollout-operator".to_owned(),
                github_name: Some("Rollout Operator".to_owned()),
                github_avatar_url: None,
            },
        )
        .await
        .expect("bootstrap");

        migrations::run_postgres_migrations(
            &db_url,
            &MigrationContext {
                designated_operator_user_id: Some(contract_support::DESIGNATED.to_owned()),
            },
        )
        .await
        .expect("runner");

        let pool = sqlx::PgPool::connect(&db_url).await.expect("connect");
        let applied: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE version = 142 AND success = TRUE",
        )
        .fetch_one(&pool)
        .await
        .expect("check");
        assert_eq!(applied, 1, "migration 142 must be recorded");
        let is_nullable: String = sqlx::query_scalar(
            "SELECT is_nullable FROM information_schema.columns \
             WHERE table_name = 'tasks' AND column_name = 'created_by_user_id'",
        )
        .fetch_one(&pool)
        .await
        .expect("check");
        assert_eq!(is_nullable, "NO");
        pool.close().await;
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Persisted refinement-owner recovery through schema graduation
// ═══════════════════════════════════════════════════════════════════════════

/// The contract migration must not disturb persisted refinement ownership
/// columns on tasks. A task with refinement metadata before the contract
/// must retain it after graduation.
#[tokio::test]
async fn persisted_refinement_owner_survives_schema_graduation() {
    contract_support::with_temp_database("rollout_ref_owner", |db_url| async move {
        let mut conn = PgConnection::connect(&db_url).await.expect("connect");
        contract_support::apply_prior_migrations(&mut conn).await;

        contract_support::seed_project(&mut conn, "project-1").await;
        contract_support::seed_user(&mut conn, "u-refine", false).await;
        contract_support::seed_user(&mut conn, contract_support::DESIGNATED, false).await;

        // Seed a task with free-form refinement ownership columns set.
        // (refinement_run_id/refinement_intent_id have FKs to
        // refinement_runs/refinement_dispatch_intents and are left NULL; the
        // free-form correlation columns from migration 140 are the durable
        // refinement-owner recovery surface that must survive graduation.)
        sqlx::query(
            "INSERT INTO tasks \
             (id, project_id, short_id, title, description, design, \
              labels, acceptance_criteria, memory_refs, created_by_user_id, \
              refinement_generation, refinement_round, refinement_phase, refinement_role) \
             VALUES ('t-refine', 'project-1', 'sr', 't', 'd', 'dd', \
                     '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, 'u-refine', \
                     1, 1, 'judge', 'owner')",
        )
        .execute(&mut conn)
        .await
        .expect("seed refinement task");

        contract_support::set_operator(&mut conn, contract_support::DESIGNATED).await;
        contract_support::apply_contract_migration(&mut conn).await;

        let (creator, generation, phase, role): (String, i64, String, String) = sqlx::query_as(
            "SELECT created_by_user_id, refinement_generation, \
                        refinement_phase, refinement_role \
                 FROM tasks WHERE id = 't-refine'",
        )
        .fetch_one(&mut conn)
        .await
        .expect("fetch");

        assert_eq!(creator, "u-refine", "creator must be preserved");
        assert_eq!(
            generation, 1,
            "refinement_generation must survive graduation"
        );
        assert_eq!(phase, "judge", "refinement_phase must survive graduation");
        assert_eq!(role, "owner", "refinement_role must survive graduation");
        conn.close().await.expect("close");
    })
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Legacy board-health guard retention through schema graduation
// ═══════════════════════════════════════════════════════════════════════════

/// The pre-contract legacy board-health guard regression must continue to use
/// the `EffectiveCreatorProvenance` boundary through schema graduation.
#[test]
fn legacy_board_health_guard_retains_provenance_boundary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../");
    let guard = root
        .join("server/crates/djinn-db/src/repositories/task/queries/board_health_bounds_tests.rs");
    let source = std::fs::read_to_string(&guard)
        .unwrap_or_else(|_| panic!("board-health guard source must exist: {}", guard.display()));

    assert!(
        source.contains("EffectiveCreatorProvenance"),
        "board-health guard must use EffectiveCreatorProvenance after schema graduation"
    );
    for legacy in LEGACY_CREATE_METHODS {
        assert!(
            !source.contains(legacy),
            "board-health guard must not use legacy API {legacy}"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Concrete release repository typing and dead-helper removal
// ═══════════════════════════════════════════════════════════════════════════

/// The dead `cfg(test)` creator-less `create_with_short_id` helper must be
/// removed from the release repository.
#[test]
fn dead_create_with_short_id_helper_is_removed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../");
    let writes = root.join("server/crates/djinn-db/src/repositories/task/writes.rs");
    let source = std::fs::read_to_string(&writes).expect("writes source readable");
    assert!(
        !source.contains("fn create_with_short_id"),
        "dead cfg(test) create_with_short_id helper must be removed"
    );
}

/// The release repository must not expose any nullable-creator insert path.
#[test]
fn release_repository_typing_rejects_nullable_creator() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../");
    let writes = root.join("server/crates/djinn-db/src/repositories/task/writes.rs");
    let source = std::fs::read_to_string(&writes).expect("writes source readable");

    let direct_inserts = direct_task_insert_constants(&source);
    for (_, sql) in &direct_inserts {
        assert!(
            sql.contains("created_by_user_id"),
            "boundary INSERT must name created_by_user_id"
        );
        assert!(
            !sql.to_ascii_uppercase().contains("NULL"),
            "boundary INSERT must not bind NULL creator"
        );
    }

    assert!(
        source.contains("provenance: EffectiveCreatorProvenance<'_>"),
        "release write boundary must require EffectiveCreatorProvenance"
    );
}
