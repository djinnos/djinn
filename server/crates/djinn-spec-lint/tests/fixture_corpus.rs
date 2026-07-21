//! Checked-in, database-independent V1 lint corpus contract tests.

use std::fs;
use std::path::Path;

use djinn_spec_lint::{BodyFormat, lint};
use serde::Deserialize;
use serde_json::Value;

const FIXED_TIMESTAMP: &str = "2026-07-20T00:00:00Z";
const CHANGED_TIMESTAMP: &str = "2026-07-21T00:00:00Z";

#[derive(Debug, Deserialize)]
struct FixtureMetadata {
    source: SourceProvenance,
    #[serde(default)]
    expected_slices: Vec<ExpectedSlice>,
}

#[derive(Debug, Deserialize)]
struct SourceProvenance {
    source_proposal_short_id: Option<String>,
    source_revision_sequence: Option<u64>,
    source_revision_note: String,
    redaction_note: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedSlice {
    severity: String,
    code: String,
    slice: String,
}

fn fixture_names() -> &'static [&'static str] {
    &[
        "provenance/czd9",
        "provenance/3elq",
        "provenance/goxi",
        "synthetic/malformed_mdx",
        "synthetic/duplicate_ids",
        "synthetic/duplicate_sections",
        "synthetic/glued_tokens",
        "synthetic/delimiter_failures",
        "synthetic/unresolved_reference",
        "clean/markdown_exclusions",
        "clean/mdx_exclusions",
    ]
}

fn fixture_path(name: &str, file: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/v1")
        .join(name)
        .join(file)
}

fn body_format(metadata: &Value) -> BodyFormat {
    match metadata["body_format"].as_str() {
        Some("markdown") => BodyFormat::Markdown,
        Some("mdx") => BodyFormat::Mdx,
        other => panic!("fixture has invalid body_format: {other:?}"),
    }
}

#[test]
fn fixture_corpus_pins_full_v1_results_and_source_slices() {
    for name in fixture_names() {
        let metadata_path = fixture_path(name, "metadata.json");
        let metadata_value: Value = serde_json::from_str(
            &fs::read_to_string(&metadata_path).expect("fixture metadata is checked in"),
        )
        .expect("fixture metadata is valid JSON");
        let metadata: FixtureMetadata = serde_json::from_value(metadata_value.clone()).unwrap();
        let body =
            fs::read_to_string(fixture_path(name, "body.md")).expect("fixture body is checked in");
        let result = lint(&body, body_format(&metadata_value), FIXED_TIMESTAMP);
        result.validate_for_body(&body).unwrap();

        // Provenance is data, not a filename convention. Redacted reproductions
        // must name their source and explain both the redaction and any absent
        // historic revision sequence without looking up a live database.
        if name.starts_with("provenance/") {
            assert!(metadata.source.source_proposal_short_id.is_some(), "{name}");
            assert!(!metadata.source.redaction_note.trim().is_empty(), "{name}");
            assert!(
                metadata.source.source_revision_sequence.is_some()
                    || !metadata.source.source_revision_note.trim().is_empty(),
                "{name}: revision sequence or its documented availability is required"
            );
        }

        let expected: Value = serde_json::from_str(
            &fs::read_to_string(fixture_path(name, "expected.json"))
                .expect("expected snapshot is checked in"),
        )
        .expect("expected snapshot is valid JSON");
        let actual = serde_json::to_value(&result).unwrap();
        assert_eq!(actual, expected, "full V1 snapshot drifted for {name}");

        let slices = result
            .errors
            .iter()
            .chain(&result.warnings)
            .map(|violation| {
                serde_json::json!({
                    "severity": match violation.severity {
                        djinn_spec_lint::Severity::Error => "error",
                        djinn_spec_lint::Severity::Warning => "warning",
                    },
                    "code": violation.code,
                    "slice": &body[violation.span.start..violation.span.end],
                })
            })
            .collect::<Vec<_>>();
        let expected_slices = metadata.expected_slices.into_iter().map(|slice| {
            serde_json::json!({"severity": slice.severity, "code": slice.code, "slice": slice.slice})
        }).collect::<Vec<_>>();
        assert_eq!(slices, expected_slices, "span slices drifted for {name}");
    }
}

#[test]
fn fixture_results_are_byte_deterministic_except_for_supplied_timestamp() {
    for name in fixture_names() {
        let metadata: Value =
            serde_json::from_str(&fs::read_to_string(fixture_path(name, "metadata.json")).unwrap())
                .unwrap();
        let body = fs::read_to_string(fixture_path(name, "body.md")).unwrap();
        let format = body_format(&metadata);
        let first = lint(&body, format, FIXED_TIMESTAMP);
        let repeated = lint(&body, format, FIXED_TIMESTAMP);
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&repeated).unwrap(),
            "{name}"
        );

        let changed = lint(&body, format, CHANGED_TIMESTAMP);
        assert_ne!(first.checked_at, changed.checked_at, "{name}");
        let mut timestamp_normalized = serde_json::to_value(changed).unwrap();
        timestamp_normalized["checked_at"] = Value::String(FIXED_TIMESTAMP.into());
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            timestamp_normalized,
            "{name}"
        );
    }
}
