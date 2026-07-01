use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::{CiFailureContextRequest, GitHubApiClient};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

use super::seed_installation_token;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_run_jobs_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/actions/runs/123/jobs"))
        .and(header("Authorization", "Bearer ghs_test_install"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobs": [{
                "id": 7,
                "name": "Tests",
                "status": "completed",
                "conclusion": "failure",
                "html_url": "https://github.com/jobs/7",
                "workflow_name": "ci.yml",
                "steps": [{
                    "name": "cargo test",
                    "status": "completed",
                    "conclusion": "failure",
                    "number": 1
                }]
            }]
        })))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let jobs = client
        .list_run_jobs("djinnos", "server", 123)
        .await
        .unwrap();

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].name, "Tests");
    assert_eq!(jobs[0].steps[0].name, "cargo test");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_check_run_annotations_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/check-runs/555/annotations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            {
                "path": "src/lib.rs",
                "start_line": 10,
                "end_line": 10,
                "annotation_level": "failure",
                "message": "expected type",
                "title": "rustc"
            }
        ])))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let annotations = client
        .get_check_run_annotations("djinnos", "server", 555)
        .await
        .unwrap();

    assert_eq!(annotations.len(), 1);
    assert_eq!(annotations[0].annotation_level, "failure");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_job_logs_success() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/actions/jobs/77/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_string("line 1\nline 2\n"))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let logs = client.get_job_logs("djinnos", "server", 77).await.unwrap();

    assert!(logs.contains("line 1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_failure_context_bundle_extracts_workflow_command_setup_and_log_tail() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    let workflow = r#"
name: Generic Quality
on: [pull_request]
jobs:
  dynamic-check:
    name: Repository Defined Check
    runs-on: ubuntu-latest
    steps:
      - name: Checkout repository
        uses: actions/checkout@v4
      - name: Prepare local fixture
        run: |
          printf 'alpha' > generated-input.txt
          printf 'beta' >> generated-input.txt
      - name: Validate generated artifact
        run: |
          ./tools/validate-generated-artifact --input generated-input.txt
          ./tools/validate-generated-artifact --strict
      - name: Later step
        run: echo not reached
"#;
    let encoded_workflow = BASE64.encode(workflow);

    Mock::given(method("GET"))
        .and(path("/repos/example/project/actions/runs/4242"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 4242,
            "workflow_id": 99,
            "name": "Generic Quality",
            "path": ".github/workflows/generic-quality.yml",
            "head_sha": "abc123def456",
            "status": "completed",
            "conclusion": "failure"
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/example/project/actions/runs/4242/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobs": [{
                "id": 777,
                "run_id": 4242,
                "name": "Repository Defined Check",
                "status": "completed",
                "conclusion": "failure",
                "html_url": "https://github.com/example/project/actions/jobs/777",
                "workflow_name": "Generic Quality",
                "steps": [
                    {"name": "Checkout repository", "status": "completed", "conclusion": "success", "number": 1},
                    {"name": "Prepare local fixture", "status": "completed", "conclusion": "success", "number": 2},
                    {"name": "Validate generated artifact", "status": "completed", "conclusion": "failure", "number": 3}
                ]
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(
            "/repos/example/project/contents/.github/workflows/generic-quality.yml",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "encoding": "base64",
            "content": encoded_workflow
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/example/project/actions/jobs/777/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "old setup output\nValidate generated artifact\nfirst relevant failure line\nsecond relevant failure line\n",
        ))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let bundle = client
        .build_ci_failure_context_bundle(CiFailureContextRequest {
            owner: "example".to_string(),
            repo: "project".to_string(),
            head_sha: "abc123def456".to_string(),
            required_check_name: "Generic Quality / Repository Defined Check".to_string(),
            workflow_run_id: Some(4242),
            workflow_id: None,
            job_id: None,
            workflow_path: None,
        })
        .await
        .unwrap();

    assert_eq!(bundle.observed_head_sha, "abc123def456");
    assert_eq!(bundle.job_name, "Repository Defined Check");
    assert_eq!(bundle.failing_step_name, "Validate generated artifact");
    assert_eq!(bundle.failing_step_number, 3);
    assert!(
        bundle
            .step_script
            .contains("./tools/validate-generated-artifact --input")
    );
    assert!(
        bundle
            .step_script
            .contains("./tools/validate-generated-artifact --strict")
    );
    assert_eq!(bundle.setup_steps.len(), 2);
    assert_eq!(
        bundle.setup_steps[0].uses.as_deref(),
        Some("actions/checkout@v4")
    );
    assert!(
        bundle.setup_steps[1]
            .command
            .as_deref()
            .unwrap()
            .contains("printf 'alpha'")
    );
    assert!(bundle.log_tail.contains("first relevant failure line"));
    assert!(!bundle.log_tail.contains("old setup output"));
}

#[test]
fn ci_failure_extraction_engine_has_no_command_registry_constants() {
    let engine = concat!(include_str!("../checks.rs"), include_str!("../types.rs"),);
    for forbidden in ["scripts/check-file-size.sh", "cargo fmt", "clippy"] {
        assert!(
            !engine.contains(forbidden),
            "CI failure extraction must derive commands from workflow data, not hardcode {forbidden}"
        );
    }
}
