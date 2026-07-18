//! Static inventory only: this test makes no behavioral claim.
//!
//! It catches new production `match` sites with `StreamEvent` arms that need an
//! explicit owner-and-arm classification. Behavioral assertions remain in each
//! owner crate.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const FIXTURE: &str = include_str!("fixtures/stream_event_consumer_audit.tsv");

fn is_test_only(path: &Path) -> bool {
    path.components().any(|part| part.as_os_str() == "tests")
        || path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name == "tests.rs" || name == "test_helpers.rs" || name.ends_with("_tests.rs")
        })
}

fn is_match_keyword(source: &str, offset: usize) -> bool {
    let before = source[..offset].chars().next_back();
    let after = source[offset + "match".len()..].chars().next();
    !before.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
        && after.is_some_and(char::is_whitespace)
}

fn closing_brace(source: &str, opening: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (relative, character) in source[opening..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(opening + relative);
                }
            }
            _ => {}
        }
    }
    None
}

/// Match patterns begin an arm line with `StreamEvent` (or its `Result` /
/// `Option` wrapper). This excludes producers that construct a `StreamEvent`
/// in a different wire-event match body.
fn has_stream_event_arm(match_body: &str) -> bool {
    match_body.lines().any(|line| {
        let pattern = line.trim_start().trim_start_matches('|').trim_start();
        pattern.starts_with("StreamEvent::")
            || pattern.starts_with("Ok(StreamEvent::")
            || pattern.starts_with("Some(StreamEvent::")
    })
}

fn enclosing_function(source: &str, offset: usize) -> &str {
    source[..offset]
        .rmatch_indices("fn ")
        .find_map(|(function_offset, _)| {
            source[function_offset + 3..]
                .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .next()
                .filter(|name| !name.is_empty())
        })
        .unwrap_or("module")
}

/// Unit-test scopes are fixtures/assertions rather than production match sites.
fn test_scope_ranges(source: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let mut search_from = 0;
    while let Some(relative_attribute) = source[search_from..].find("#[cfg(test)]") {
        let attribute = search_from + relative_attribute;
        let Some(opening) = source[attribute..].find('{').map(|i| attribute + i) else {
            break;
        };
        let Some(closing) = closing_brace(source, opening) else {
            break;
        };
        ranges.push(attribute..closing + 1);
        search_from = closing + 1;
    }
    ranges
}

fn collect_match_sites(root: &Path, repo_root: &Path, found: &mut BTreeSet<String>) {
    for entry in fs::read_dir(root).expect("read source directory") {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if path.is_dir() {
            collect_match_sites(&path, repo_root, found);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && !is_test_only(&path)
        {
            let source = fs::read_to_string(&path).expect("read Rust source");
            let relative_path = path
                .strip_prefix(repo_root)
                .expect("path below repository")
                .to_string_lossy()
                .replace('\\', "/");
            let test_scopes = test_scope_ranges(&source);
            let mut search_from = 0;
            let mut function_match_counts = std::collections::BTreeMap::new();
            while let Some(relative_match) = source[search_from..].find("match") {
                let match_offset = search_from + relative_match;
                search_from = match_offset + "match".len();
                if !is_match_keyword(&source, match_offset)
                    || test_scopes
                        .iter()
                        .any(|scope| scope.contains(&match_offset))
                {
                    continue;
                }
                let Some(opening) = source[search_from..].find('{').map(|i| search_from + i) else {
                    continue;
                };
                let Some(closing) = closing_brace(&source, opening) else {
                    continue;
                };
                if !has_stream_event_arm(&source[opening..=closing]) {
                    continue;
                }
                let function = enclosing_function(&source, match_offset);
                let count = function_match_counts
                    .entry(function.to_owned())
                    .or_insert(0usize);
                *count += 1;
                found.insert(format!("{relative_path}::{function}#{count}"));
            }
        }
    }
}

#[test]
fn checked_in_classification_covers_every_production_stream_event_match() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let expected = FIXTURE
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            line.split_once('\t')
                .expect("site identifier and arm classification")
                .0
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    collect_match_sites(&repo_root.join("server/crates"), &repo_root, &mut actual);
    collect_match_sites(&repo_root.join("server/src"), &repo_root, &mut actual);

    assert_eq!(
        actual, expected,
        "update the classification fixture for every production StreamEvent match site; this audit is not behavioral coverage"
    );
    assert!(FIXTURE.contains("drain_provider_turn"));
    assert!(FIXTURE.contains("grouped ignore arm"));
    assert!(FIXTURE.contains("wildcard"));
    assert!(
        FIXTURE.contains("no behavioral claim")
            || include_str!("stream_event_consumer_audit.rs").contains("no behavioral claim")
    );
}
