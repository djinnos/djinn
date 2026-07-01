use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::github_api::{
    GitHubApiClient, RequiredCheckReproduction, RequiredCheckUnreproducibleReason,
};

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
async fn required_check_reproduction_context_extracts_command_and_setup() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/commits/head-sha/check-runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 1,
            "check_runs": [{
                "id": 9001,
                "name": "Quality Gate / Unit Tests",
                "status": "completed",
                "conclusion": "failure",
                "html_url": "https://github.com/checks/9001"
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/actions/runs"))
        .and(query_param("event", "pull_request"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "workflow_runs": [{
                "id": 1234,
                "name": "Quality Gate",
                "head_branch": "feature-branch",
                "head_sha": "head-sha",
                "status": "completed",
                "conclusion": "failure"
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/actions/runs/1234/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "jobs": [{
                "id": 77,
                "name": "Unit Tests",
                "status": "completed",
                "conclusion": "failure",
                "html_url": "https://github.com/jobs/77",
                "workflow_name": "quality-gate.yml",
                "steps": [
                    {
                        "name": "Install project tools",
                        "status": "completed",
                        "conclusion": "success",
                        "number": 1
                    },
                    {
                        "name": "Run unit tests",
                        "status": "completed",
                        "conclusion": "failure",
                        "number": 2
                    }
                ]
            }]
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/actions/jobs/77/logs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "2026-01-01T00:00:00.000Z ##[group]Run setup tools\n\
             2026-01-01T00:00:01.000Z setup output\n\
             2026-01-01T00:00:02.000Z ##[endgroup]\n\
             2026-01-01T00:00:03.000Z ##[group]Run run repo unit test command\n\
             2026-01-01T00:00:04.000Z assertion failed\n",
        ))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let result = client
        .required_check_reproduction_context(
            "djinnos",
            "server",
            "head-sha",
            "Quality Gate / Unit Tests",
        )
        .await
        .unwrap();

    let RequiredCheckReproduction::Reproducible(context) = result else {
        panic!("expected reproducible context");
    };
    assert_eq!(context.observed_head_sha, "head-sha");
    assert_eq!(context.workflow_run_id, 1234);
    assert_eq!(context.job.id, 77);
    assert_eq!(context.job.name, "Unit Tests");
    assert_eq!(context.failing_step.name, "Run unit tests");
    assert_eq!(context.command, "run repo unit test command");
    assert_eq!(context.setup_steps.len(), 1);
    assert_eq!(context.setup_steps[0].command, "setup tools");
    assert!(context.log_tail.contains("assertion failed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_check_reproduction_context_returns_unreproducible_for_unmappable_check() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();

    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/commits/head-sha/check-runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 1,
            "check_runs": [{
                "id": 42,
                "name": "Required Gate",
                "status": "completed",
                "conclusion": "failure",
                "html_url": "https://github.com/checks/42"
            }]
        })))
        .mount(&server)
        .await;

    for event in ["pull_request", "push", "merge_group", "workflow_dispatch"] {
        Mock::given(method("GET"))
            .and(path("/repos/djinnos/server/actions/runs"))
            .and(query_param("event", event))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "workflow_runs": [{
                    "id": 100,
                    "name": event,
                    "head_sha": "some-other-sha",
                    "status": "completed",
                    "conclusion": "failure"
                }]
            })))
            .expect(1)
            .mount(&server)
            .await;
    }

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let result = client
        .required_check_reproduction_context("djinnos", "server", "head-sha", "Required Gate")
        .await
        .unwrap();

    let RequiredCheckReproduction::Unreproducible(unreproducible) = result else {
        panic!("expected unreproducible result");
    };
    assert_eq!(unreproducible.required_check_name, "Required Gate");
    assert_eq!(unreproducible.observed_head_sha, "head-sha");
    assert_eq!(
        unreproducible.reason,
        RequiredCheckUnreproducibleReason::WorkflowRunNotFound
    );
}
