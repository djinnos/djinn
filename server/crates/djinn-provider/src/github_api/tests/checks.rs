use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::GitHubApiClient;

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
