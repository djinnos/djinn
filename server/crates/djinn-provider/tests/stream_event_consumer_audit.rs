//! Static inventory only: this test makes no behavioral claim.
//!
//! It catches new production `StreamEvent::` references that need an explicit
//! owner classification. Behavioral assertions remain in each owner crate.

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

fn collect_matches(root: &Path, repo_root: &Path, found: &mut BTreeSet<String>) {
    for entry in fs::read_dir(root).expect("read source directory") {
        let entry = entry.expect("directory entry");
        let path = entry.path();
        if path.is_dir() {
            collect_matches(&path, repo_root, found);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && !is_test_only(&path)
            && fs::read_to_string(&path)
                .expect("read Rust source")
                .contains("StreamEvent::")
        {
            found.insert(
                path.strip_prefix(repo_root)
                    .expect("path below repository")
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
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
                .expect("path and classification")
                .0
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    collect_matches(&repo_root.join("server/crates"), &repo_root, &mut actual);
    collect_matches(&repo_root.join("server/src"), &repo_root, &mut actual);

    assert_eq!(
        actual, expected,
        "update the classification fixture for every production match; this audit is not behavioral coverage"
    );
    assert!(FIXTURE.contains("drain_provider_turn"));
    assert!(FIXTURE.contains("grouped ignore arm"));
    assert!(
        FIXTURE.contains("no behavioral claim")
            || include_str!("stream_event_consumer_audit.rs").contains("no behavioral claim")
    );
}
