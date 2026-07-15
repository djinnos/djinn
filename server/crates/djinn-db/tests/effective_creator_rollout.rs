//! Callsite-exact expand-phase inventory gate for production task writers.

use std::{fs, path::Path};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Callsite {
    path: String,
    symbol: String,
    provenance: String,
}

const METHODS: &[&str] = &[
    "create_in_project",
    "create_in_project_with_blockers",
    "create_in_project_with_provenance",
];

#[test]
fn production_task_writers_match_the_callsite_inventory_and_provenance_boundary() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/effective_creator_producers.json"))
            .expect("valid inventory");
    let mut expected = fixture["callsites"]
        .as_array()
        .expect("callsites array")
        .iter()
        .map(|entry| Callsite {
            path: entry["path"].as_str().expect("path").to_owned(),
            symbol: entry["symbol"].as_str().expect("symbol").to_owned(),
            provenance: entry["provenance"].as_str().expect("provenance").to_owned(),
        })
        .collect::<Vec<_>>();
    assert!(
        !expected.is_empty(),
        "production inventory must not be empty"
    );

    let mut discovered = Vec::new();
    for scope in [
        "server/crates/djinn-agent/src",
        "server/crates/djinn-control-plane/src",
        "server/crates/djinn-coordinator/src",
        "server/crates/djinn-slot/src",
    ] {
        discover(&root.join(scope), &mut |path| {
            if is_test_path(path) {
                return;
            }
            let source = fs::read_to_string(path).expect("production Rust source");
            let relative = path
                .strip_prefix(&root)
                .expect("source below root")
                .to_string_lossy()
                .replace('\\', "/");
            for call in calls(&source) {
                assert_ne!(
                    call.method, "create_in_project",
                    "legacy/default task creation at {}:{} ({})",
                    relative, call.line, call.symbol
                );
                assert!(
                    call.text.contains("EffectiveCreatorProvenance"),
                    "missing provenance boundary at {}:{} ({})",
                    relative,
                    call.line,
                    call.symbol
                );
                assert!(
                    !call.text.contains("EffectiveCreatorProvenance::default"),
                    "default provenance at {}:{} ({})",
                    relative,
                    call.line,
                    call.symbol
                );
                assert!(
                    !call.enclosing_function.contains("set_created_by_user_id"),
                    "post-insert attribution at {}:{} ({})",
                    relative,
                    call.line,
                    call.symbol
                );
                discovered.push(Callsite {
                    path: relative.clone(),
                    symbol: call.symbol,
                    provenance: provenance_kind(&call.text)
                        .unwrap_or_else(|| {
                            panic!("unclassified provenance at {}:{}", relative, call.line)
                        })
                        .to_owned(),
                });
            }
        });
    }
    let writes =
        fs::read_to_string(root.join("server/crates/djinn-db/src/repositories/task/writes.rs"))
            .expect("task writer repository");
    assert!(writes.contains("let created_by_user_id = resolve_effective_creator("));
    assert!(writes.contains("created_by_user_id)"));
    expected.sort();
    discovered.sort();
    assert_eq!(
        discovered, expected,
        "every production writer must be inventoried and stale entries are rejected"
    );
}

struct Invocation {
    method: String,
    symbol: String,
    line: usize,
    text: String,
    /// The complete enclosing function, not merely the create invocation.
    /// A creator patch follows the invocation and must be checked here.
    enclosing_function: String,
}

fn calls(source: &str) -> Vec<Invocation> {
    let code = strip_comments_and_strings(source);
    let tests = inline_test_ranges(&code);
    let mut result = Vec::new();
    for method in METHODS {
        let needle = format!(".{method}(");
        let mut start = 0;
        while let Some(found) = code[start..].find(&needle) {
            let offset = start + found;
            start = offset + needle.len();
            if tests.iter().any(|range| range.contains(&offset))
                || function_has_test_attribute(&code, offset)
            {
                continue;
            }
            let open = offset + needle.len() - 1;
            let end = matching(&code, open, b'(', b')').expect("balanced task creation invocation");
            let (symbol, enclosing_function) = enclosing_function(&code, offset);
            result.push(Invocation {
                method: (*method).to_owned(),
                symbol,
                line: code[..offset].bytes().filter(|byte| *byte == b'\n').count() + 1,
                text: code[offset..=end].to_owned(),
                enclosing_function,
            });
        }
    }
    result
}

fn function_has_test_attribute(code: &str, offset: usize) -> bool {
    let before = &code[..offset];
    let function = ["\n    async fn ", "\n    fn ", "\nasync fn ", "\nfn "]
        .iter()
        .filter_map(|needle| before.rfind(needle))
        .max();
    function.is_some_and(|function| {
        let attributes = &code[function.saturating_sub(512)..function];
        attributes.contains("#[test]") || attributes.contains("#[tokio::test")
    })
}

fn provenance_kind(call: &str) -> Option<&'static str> {
    if call.contains("explicit_user_id:") && !call.contains("explicit_user_id: None") {
        Some("explicit")
    } else if call.contains("source_task_id:") && !call.contains("source_task_id: None") {
        Some("source_task")
    } else if call.contains("proposal_id: Some") {
        Some("proposal")
    } else if call.contains("Some(epic_id)") || call.contains("Some(&epic.id)") {
        Some("epic")
    } else {
        None
    }
}

fn discover(root: &Path, visit: &mut impl FnMut(&Path)) {
    for entry in fs::read_dir(root).expect("production source directory") {
        let path = entry.expect("source directory entry").path();
        if path.is_dir() {
            discover(&path, visit);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            visit(&path);
        }
    }
}

/// Only explicit test paths and explicit `#[cfg(test)] mod` blocks are excluded.
fn is_test_path(path: &Path) -> bool {
    let path = path.to_string_lossy();
    path.contains("/tests/")
        || path.ends_with("_tests.rs")
        || path.ends_with("/test_helpers.rs")
        || path.contains("/test_support/")
}

fn inline_test_ranges(code: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut search = 0;
    while let Some(found) = code[search..].find("#[cfg(test)]") {
        let attribute = search + found;
        let after = &code[attribute + "#[cfg(test)]".len()..];
        let Some(module_relative) = after.find("mod ") else {
            search = attribute + 1;
            continue;
        };
        if after[..module_relative].contains(';') {
            search = attribute + 1;
            continue;
        }
        let module = attribute + "#[cfg(test)]".len() + module_relative;
        let Some(open_relative) = code[module..].find('{') else {
            break;
        };
        let open = module + open_relative;
        let end = matching(code, open, b'{', b'}').unwrap_or(code.len() - 1);
        ranges.push(attribute..end + 1);
        search = end + 1;
    }
    ranges
}

fn enclosing_function(code: &str, offset: usize) -> (String, String) {
    let mut declaration = None;
    let mut line_start = 0;
    for line in code[..offset].split_inclusive('\n') {
        let trimmed = line.trim_start();
        let candidate = trimmed
            .strip_prefix("pub(crate) async fn ")
            .or_else(|| trimmed.strip_prefix("pub(super) async fn "))
            .or_else(|| trimmed.strip_prefix("pub async fn "))
            .or_else(|| trimmed.strip_prefix("async fn "))
            .or_else(|| trimmed.strip_prefix("pub(super) fn "))
            .or_else(|| trimmed.strip_prefix("pub(crate) fn "))
            .or_else(|| trimmed.strip_prefix("pub fn "))
            .or_else(|| trimmed.strip_prefix("fn "));
        if let Some(candidate) = candidate {
            let symbol = candidate
                .split(|character: char| character == '(' || character.is_whitespace())
                .next()
                .unwrap_or("<module>")
                .to_owned();
            declaration = Some((line_start + line.len() - trimmed.len(), symbol));
        }
        line_start += line.len();
    }
    let Some((start, symbol)) = declaration else {
        return ("<module>".to_owned(), code.to_owned());
    };
    let open = code[start..]
        .find('{')
        .map(|relative| start + relative)
        .expect("function containing task creation has a body");
    let end = matching(code, open, b'{', b'}').expect("balanced function containing task creation");
    (symbol, code[start..=end].to_owned())
}

fn matching(code: &str, open: usize, left: u8, right: u8) -> Option<usize> {
    let mut depth = 0;
    for (index, byte) in code.as_bytes().iter().enumerate().skip(open) {
        if *byte == left {
            depth += 1;
        } else if *byte == right {
            depth -= 1;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn strip_comments_and_strings(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut result = bytes.to_vec();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"//") {
            let end = bytes[index..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map(|length| index + length)
                .unwrap_or(bytes.len());
            result[index..end].fill(b' ');
            index = end;
        } else if bytes[index..].starts_with(b"/*") {
            let end = bytes[index + 2..]
                .windows(2)
                .position(|window| window == b"*/")
                .map(|length| index + length + 4)
                .unwrap_or(bytes.len());
            for byte in &mut result[index..end] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
            index = end;
        } else if bytes[index] == b'\"' {
            let mut end = index + 1;
            while end < bytes.len() {
                if bytes[end] == b'\\' {
                    end += 2;
                } else if bytes[end] == b'\"' {
                    end += 1;
                    break;
                } else {
                    end += 1;
                }
            }
            for byte in &mut result[index..end.min(bytes.len())] {
                if *byte != b'\n' {
                    *byte = b' ';
                }
            }
            index = end;
        } else {
            index += 1;
        }
    }
    String::from_utf8(result).expect("Rust source remains UTF-8")
}

#[cfg(test)]
mod tests {
    use super::calls;

    #[test]
    fn post_insert_attribution_is_detected_per_enclosing_function() {
        let source = r#"
async fn valid_writer() {
    repo.create_in_project_with_provenance(
        EffectiveCreatorProvenance { explicit_user_id: Some(user_id) },
    ).await?;
}

async fn patched_writer() {
    let task = repo.create_in_project_with_provenance(
        EffectiveCreatorProvenance { explicit_user_id: Some(user_id) },
    ).await?;
    repo.set_created_by_user_id(&task.id, user_id).await?;
}
"#;

        let calls = calls(source);
        let valid = calls
            .iter()
            .find(|call| call.symbol == "valid_writer")
            .expect("valid call discovered");
        let patched = calls
            .iter()
            .find(|call| call.symbol == "patched_writer")
            .expect("patched call discovered");

        assert!(!valid.enclosing_function.contains("set_created_by_user_id"));
        assert!(
            patched
                .enclosing_function
                .contains("set_created_by_user_id")
        );
    }
}
