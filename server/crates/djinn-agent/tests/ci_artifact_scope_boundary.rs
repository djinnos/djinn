//! Agent-layer scope boundary for the `ci_artifact` tool.
//!
//! These integration tests exercise the internal implementation
//! (resolution, listing, and bounded in-memory ZIP rendering) through
//! the `test-support` gateway — not a duplicate test-only path.

use djinn_agent::test_helpers::{
    fetch_artifact_for_test, list_artifacts_for_test, render_ci_artifact_zip_for_test,
};
use djinn_provider::github_api::GitHubApiClient;
use std::io::Write;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use zip::write::SimpleFileOptions;

const OWNER: &str = "task-owner";
const REPO: &str = "task-repository";

fn client(server: &MockServer) -> GitHubApiClient {
    GitHubApiClient::for_user_token_with_base_url("token".into(), server.uri())
}

fn run_json(run_id: u64) -> serde_json::Value {
    serde_json::json!({"id": run_id, "head_sha": "sha", "status": "completed", "conclusion": "success"})
}

fn artifacts_json(name: &str) -> serde_json::Value {
    serde_json::json!({"total_count": 1, "artifacts": [{
        "id": 77, "name": name, "size_in_bytes": 123,
        "expired": false, "expires_at": "2030-01-01T00:00:00Z"
    }]})
}

fn zip(entries: Vec<(&str, Vec<u8>)>) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (n, b) in entries {
        w.start_file(n, SimpleFileOptions::default()).unwrap();
        w.write_all(&b).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// An explicit `run_id` from a foreign (inaccessible) repository must be
/// rejected by the repository-scoped `get_workflow_run` verification before
/// any artifact listing or download is attempted.
#[tokio::test]
async fn foreign_run_id_stops_before_artifact_io() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/actions/runs/999")))
        .respond_with(ResponseTemplate::new(404).set_body_string("foreign run"))
        .mount(&server)
        .await;
    let error = list_artifacts_for_test(&client(&server), OWNER, REPO, 999)
        .await
        .unwrap_err();
    assert!(error.contains("not accessible"));
    // Only the workflow-run verification request should have been made —
    // no artifact listing or download should follow.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url.path(),
        format!("/repos/{OWNER}/{REPO}/actions/runs/999")
    );
}

/// Binary entries (NUL bytes, control bytes, invalid UTF-8) must be
/// represented as metadata-only `[skipped: …]` markers in the rendered
/// report — the body bytes of binary content must never appear in the
/// output.
#[tokio::test]
async fn binary_entries_return_metadata_without_body_bytes() {
    let binary_body = b"\x00\x01\x02\x03binary\xff\xfe";
    let bytes = zip(vec![("data.bin", binary_body.to_vec())]);
    let report = render_ci_artifact_zip_for_test(&bytes).unwrap();
    // The path appears as a header.
    assert!(report.contains("data.bin"));
    // The entry is skipped with a reason.
    assert!(
        report.contains("[skipped:"),
        "binary entry must be skipped: {report}"
    );
    // The raw binary body bytes must not appear in the text report.
    let body_str = String::from_utf8_lossy(binary_body);
    assert!(
        !report.contains(body_str.as_ref()),
        "binary body bytes must not leak into the report"
    );
}

/// Text entries rendered from a generic artifact must not introduce
/// format-specific conventions (no JUnit XML parsing, no coverage JSON
/// extraction, no file-name-based lane/type inference). The output is
/// plain text with path headers, preserving archive order.
#[tokio::test]
async fn generic_text_has_no_format_or_name_conventions() {
    // A generic text file with an arbitrary name — no special parsing
    // should be applied regardless of the file extension or content.
    let text = "line one\nline two\nline three\n";
    let bytes = zip(vec![
        ("results.junit.xml", text.as_bytes().to_vec()),
        ("coverage.lcov", text.as_bytes().to_vec()),
        ("plain.log", text.as_bytes().to_vec()),
    ]);
    let report = render_ci_artifact_zip_for_test(&bytes).unwrap();

    // All three paths appear as headers in archive order.
    let junit_pos = report.find("results.junit.xml").unwrap();
    let lcov_pos = report.find("coverage.lcov").unwrap();
    let log_pos = report.find("plain.log").unwrap();
    assert!(junit_pos < lcov_pos, "archive order must be preserved");
    assert!(lcov_pos < log_pos, "archive order must be preserved");

    // The text body is rendered verbatim — no XML tags stripped, no
    // coverage percentages extracted, no format-specific formatting.
    assert!(
        report.contains("line one\nline two\nline three\n"),
        "text must be rendered verbatim without format-specific parsing: {report}"
    );

    // No format-specific wrapper or convention markers.
    assert!(
        !report.to_lowercase().contains("test suite"),
        "no JUnit-specific parsing conventions"
    );
    assert!(
        !report.to_lowercase().contains("coverage:"),
        "no coverage-specific parsing conventions"
    );
}

// Also verify that a full fetch round-trip through the internal
// implementation renders text content (not just the ZIP renderer in
// isolation).
#[tokio::test]
async fn fetch_renders_text_content_through_internal_path() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/actions/runs/10")))
        .respond_with(ResponseTemplate::new(200).set_body_json(run_json(10)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{OWNER}/{REPO}/actions/runs/10/artifacts"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(artifacts_json("report")))
        .mount(&server)
        .await;
    let zip_bytes = zip(vec![("output.txt", b"hello world\n".to_vec())]);
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{OWNER}/{REPO}/actions/artifacts/77/zip"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(zip_bytes))
        .mount(&server)
        .await;
    let result = fetch_artifact_for_test(&client(&server), OWNER, REPO, 10, "report")
        .await
        .unwrap();
    assert!(result.contains("hello world"));
}
