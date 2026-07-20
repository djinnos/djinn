use super::*;
use djinn_provider::github_api::{ActionsJob, ActionsJobStep, WorkflowRun};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OWNER: &str = "named-owner";
const REPO: &str = "named-repository";

fn mock_client(server: &MockServer) -> GitHubApiClient {
    GitHubApiClient::for_user_token_with_base_url("test-token".into(), server.uri())
}

fn run_json(id: u64, conclusion: Option<&str>, branch: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "id": id, "head_sha": format!("sha-{id}"), "head_branch": branch,
        "status": if conclusion.is_some() { "completed" } else { "in_progress" },
        "conclusion": conclusion
    })
}

fn jobs_json(conclusion: &str) -> serde_json::Value {
    serde_json::json!({"jobs": [{
        "id": 900, "name": "tests", "status": "completed",
        "conclusion": conclusion, "html_url": "https://example.test/jobs/900", "steps": []
    }]})
}

fn run(id: u64, conclusion: Option<&str>, head_branch: Option<&str>) -> WorkflowRun {
    WorkflowRun {
        id,
        workflow_id: None,
        name: None,
        path: None,
        head_branch: head_branch.map(str::to_string),
        head_sha: format!("sha-{id}"),
        status: Some("completed".to_string()),
        conclusion: conclusion.map(str::to_string),
    }
}

fn job(id: u64, name: &str, conclusion: Option<&str>) -> ActionsJob {
    ActionsJob {
        id,
        run_id: Some(1),
        name: name.to_string(),
        status: "completed".to_string(),
        conclusion: conclusion.map(str::to_string),
        html_url: format!("https://example.test/job/{id}"),
        workflow_name: None,
        steps: Vec::new(),
    }
}

fn step(name: &str, conclusion: Option<&str>) -> ActionsJobStep {
    ActionsJobStep {
        name: name.to_string(),
        status: "completed".to_string(),
        conclusion: conclusion.map(str::to_string),
        number: 1,
    }
}

// ── select_failing_run ────────────────────────────────────────────────
#[test]
fn select_failing_run_returns_newest_failure() {
    // Newest-first: a passing run in front must not shadow the failing one.
    let runs = vec![
        run(30, Some("success"), None),
        run(20, Some("failure"), None),
        run(10, Some("failure"), None),
    ];
    assert_eq!(select_failing_run(&runs).map(|r| r.id), Some(20));
}

#[test]
fn select_failing_run_none_when_all_pass() {
    let runs = vec![run(2, Some("success"), None), run(1, None, None)];
    assert!(select_failing_run(&runs).is_none());
}

#[test]
fn select_failing_run_includes_timed_out_and_cancelled() {
    assert_eq!(
        select_failing_run(&[run(5, Some("timed_out"), None)]).map(|r| r.id),
        Some(5)
    );
    assert_eq!(
        select_failing_run(&[run(6, Some("cancelled"), None)]).map(|r| r.id),
        Some(6)
    );
}

#[test]
fn implicit_run_requires_a_failing_workflow_conclusion() {
    // A recorded merge-queue run can have a failed completed job while the
    // enclosing workflow is still running. It must not be selected until
    // GitHub reports a failure-flavor workflow conclusion.
    assert!(!is_implicit_failing_run(&run(1, None, None)));
    assert!(!is_implicit_failing_run(&run(2, Some("success"), None)));
    assert!(is_implicit_failing_run(&run(3, Some("failure"), None)));
    assert!(is_implicit_failing_run(&run(4, Some("timed_out"), None)));
    assert!(is_implicit_failing_run(&run(5, Some("cancelled"), None)));
}

// ── select_merge_group_run ────────────────────────────────────────────
#[test]
fn select_merge_group_run_matches_pr_marker() {
    let runs = vec![
        run(3, Some("failure"), Some("gh-readonly-queue/main/pr-99-abc")),
        run(2, Some("failure"), Some("gh-readonly-queue/main/pr-42-def")),
        run(1, Some("failure"), Some("gh-readonly-queue/main/pr-7-xyz")),
    ];
    assert_eq!(select_merge_group_run(&runs, 42).map(|r| r.id), Some(2));
}

#[test]
fn select_merge_group_run_ignores_passing_and_foreign_prs() {
    let runs = vec![
        run(3, Some("success"), Some("gh-readonly-queue/main/pr-42-abc")),
        run(
            2,
            Some("failure"),
            Some("gh-readonly-queue/main/pr-100-def"),
        ),
    ];
    assert!(select_merge_group_run(&runs, 42).is_none());
}

#[test]
fn select_merge_group_run_does_not_confuse_pr_prefixes() {
    // `pr-4-` must not match PR 42's `pr-42-` branch.
    let runs = vec![run(
        1,
        Some("failure"),
        Some("gh-readonly-queue/main/pr-42-abc"),
    )];
    assert!(select_merge_group_run(&runs, 4).is_none());
}

// ── select_failing_jobs ───────────────────────────────────────────────
#[test]
fn select_failing_jobs_filters_and_preserves_order() {
    let jobs = vec![
        job(1, "clippy", Some("success")),
        job(2, "tests", Some("failure")),
        job(3, "sqlx", Some("timed_out")),
        job(4, "fmt", Some("cancelled")),
        job(5, "docs", None),
    ];
    let selected = select_failing_jobs(&jobs);
    assert_eq!(
        selected.iter().map(|j| j.id).collect::<Vec<_>>(),
        vec![2, 3, 4]
    );
}

#[test]
fn select_failing_jobs_empty_when_all_green() {
    let jobs = vec![job(1, "a", Some("success")), job(2, "b", None)];
    assert!(select_failing_jobs(&jobs).is_empty());
}

// ── select_job_for_step ───────────────────────────────────────────────
#[test]
fn select_job_for_step_unique_match() {
    let mut a = job(1, "quality", Some("failure"));
    a.steps = vec![
        step("Clippy", Some("success")),
        step("Run tests", Some("failure")),
    ];
    let mut b = job(2, "build", Some("failure"));
    b.steps = vec![step("compile", Some("failure"))];
    let jobs = vec![a, b];
    assert_eq!(select_job_for_step(&jobs, "tests").map(|j| j.id), Some(1));
}

#[test]
fn select_job_for_step_ambiguous_returns_none() {
    let mut a = job(1, "a", Some("failure"));
    a.steps = vec![step("Run tests", Some("failure"))];
    let mut b = job(2, "b", Some("failure"));
    b.steps = vec![step("More tests", Some("failure"))];
    let jobs = vec![a, b];
    assert!(select_job_for_step(&jobs, "tests").is_none());
}

#[test]
fn select_job_for_step_ignores_passing_steps() {
    let mut a = job(1, "a", Some("failure"));
    a.steps = vec![step("Tests", Some("success"))];
    let jobs = vec![a];
    assert!(select_job_for_step(&jobs, "tests").is_none());
}

// ── format_failing_jobs_header ─────────────────────────────────────────
#[test]
fn format_failing_jobs_header_lists_all_jobs() {
    let jobs = vec![
        job(11, "Server Clippy", Some("failure")),
        job(22, "Server Tests", Some("timed_out")),
    ];
    let header = format_failing_jobs_header(&jobs, DiscoveryLane::MergeQueue);
    assert!(header.contains("2 failing jobs"));
    assert!(header.contains("merge-queue"));
    assert!(header.contains("**Server Clippy**, job_id=11"));
    assert!(header.contains("- Server Clippy (job_id=11) — failure"));
    assert!(header.contains("- Server Tests (job_id=22) — timed_out"));
}

// ── clean_actions_log ─────────────────────────────────────────────────
#[test]
fn clean_actions_log_strips_timestamps_and_group_markers() {
    let raw = "2026-03-24T17:10:50.0448487Z ##[group]Run cargo test\n\
               2026-03-24T17:10:51.0000000Z ##[error]boom\n\
               2026-03-24T17:10:52.0000000Z ##[endgroup]\n\
               2026-03-24T17:10:53.0000000Z plain line";
    let cleaned = clean_actions_log(raw);
    assert_eq!(cleaned, "Run cargo test\nboom\nplain line");
}

#[test]
fn clean_actions_log_preserves_non_timestamped_lines() {
    let raw = "no timestamp here\n##[warning]watch out";
    assert_eq!(clean_actions_log(raw), "no timestamp here\nwatch out");
}

// ── extract_step_log ──────────────────────────────────────────────────
#[test]
fn extract_step_log_returns_section_until_boundary() {
    let cleaned = "Run cargo build\nbuilding...\nRun cargo test\ntest output\nFAILED\nPost Run actions/checkout\ncleanup";
    let section = extract_step_log(cleaned, "cargo test").expect("step found");
    assert!(section.contains("Run cargo test"));
    assert!(section.contains("test output"));
    assert!(section.contains("FAILED"));
    assert!(!section.contains("cleanup"));
    assert!(!section.contains("building..."));
}

#[test]
fn extract_step_log_none_when_step_absent() {
    let cleaned = "Run cargo build\nbuilding...";
    assert!(extract_step_log(cleaned, "nonexistent step").is_none());
}

#[test]
fn extract_step_log_runs_to_end_without_boundary() {
    let cleaned = "Run cargo test\nline1\nline2";
    let section = extract_step_log(cleaned, "cargo test").expect("found");
    assert_eq!(section, "Run cargo test\nline1\nline2");
}

// ── render_log ────────────────────────────────────────────────────────
#[test]
fn render_log_without_step_returns_full_clean() {
    let raw = "2026-03-24T17:10:50.0448487Z hello";
    assert_eq!(render_log(raw, None), "hello");
}

#[test]
fn render_log_missing_step_falls_back_to_full_log() {
    let raw = "Run a\nout";
    let out = render_log(raw, Some("no such step"));
    assert!(out.contains("not found in the job log"));
    assert!(out.contains("Run a"));
}

// ── param deserialization ─────────────────────────────────────────────
#[test]
fn ci_job_log_params_all_optional() {
    let empty: CiJobLogParams = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(empty.job_id.is_none());
    assert!(empty.pr_number.is_none());
    assert!(empty.step.is_none());
}

#[test]
fn ci_job_log_params_parses_all_fields() {
    let p: CiJobLogParams = serde_json::from_value(serde_json::json!({
        "job_id": 12345,
        "pr_number": 42,
        "step": "Tests"
    }))
    .unwrap();
    assert_eq!(p.job_id, Some(12345));
    assert_eq!(p.pr_number, Some(42));
    assert_eq!(p.step.as_deref(), Some("Tests"));
}

#[derive(Clone, Copy, Debug)]
enum SuppressedArtifactListResponse {
    Empty,
    Forbidden,
    Missing,
    RateLimited,
    Timeout,
    Malformed,
    ProviderError,
}

impl SuppressedArtifactListResponse {
    fn response(self) -> ResponseTemplate {
        match self {
            Self::Empty => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": 0,
                "artifacts": []
            })),
            Self::Forbidden => ResponseTemplate::new(403),
            Self::Missing => ResponseTemplate::new(404),
            Self::RateLimited => ResponseTemplate::new(429),
            Self::Timeout => ResponseTemplate::new(200)
                .set_delay(ARTIFACT_HINT_TIMEOUT + Duration::from_millis(100)),
            Self::Malformed => ResponseTemplate::new(200).set_body_string("not json"),
            Self::ProviderError => ResponseTemplate::new(500),
        }
    }
}

fn successful_ci_job_log_branches() -> Vec<(&'static str, String, Option<u64>)> {
    let direct = render_log("2026-03-24T17:10:50.0448487Z direct job log", None);
    let unique_step = render_log(
        "Run focused tests\nfailed assertion\nPost cleanup",
        Some("focused"),
    );
    let single_discovered = render_log("2026-03-24T17:10:50.0448487Z single job log", None);
    let failing_jobs = vec![
        job(900, "first", Some("failure")),
        job(901, "second", Some("timed_out")),
    ];
    let multi_job = format!(
        "{}\n\n{}",
        format_failing_jobs_header(&failing_jobs, DiscoveryLane::PrHead),
        render_log("2026-03-24T17:10:50.0448487Z multi job log", None)
    );
    vec![
        ("direct job", direct, None),
        ("unique step", unique_step, Some(123)),
        ("single discovered job", single_discovered, Some(123)),
        ("multi-job header", multi_job, Some(123)),
    ]
}

async fn mount_direct_job_detail(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/actions/jobs/900")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 900, "run_id": 123, "name": "tests", "status": "completed",
            "conclusion": "failure", "html_url": "https://example.test/jobs/900"
        })))
        .mount(server)
        .await;
}

async fn assert_suppressed_artifact_hint_preserves_each_success_branch(
    response_kind: SuppressedArtifactListResponse,
) {
    let _telemetry_test_guard = TELEMETRY_TEST_LOCK.lock().await;
    for (branch, expected, known_run_id) in successful_ci_job_log_branches() {
        let server = MockServer::start().await;
        if known_run_id.is_none() {
            mount_direct_job_detail(&server).await;
        }
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/{OWNER}/{REPO}/actions/runs/123/artifacts"
            )))
            .respond_with(response_kind.response())
            .mount(&server)
            .await;
        let output = append_artifact_hint(
            &mock_client(&server),
            OWNER,
            REPO,
            "task",
            Some(900),
            known_run_id,
            expected.clone(),
        )
        .await;
        assert_eq!(
            output, expected,
            "{response_kind:?} changed {branch} output"
        );
    }
}

macro_rules! suppressed_artifact_hint_preservation_test {
    ($name:ident, $kind:expr) => {
        #[tokio::test]
        async fn $name() {
            assert_suppressed_artifact_hint_preserves_each_success_branch($kind).await;
        }
    };
}

suppressed_artifact_hint_preservation_test!(
    artifact_hint_empty_list_preserves_every_success_branch,
    SuppressedArtifactListResponse::Empty
);
suppressed_artifact_hint_preservation_test!(
    artifact_hint_403_preserves_every_success_branch,
    SuppressedArtifactListResponse::Forbidden
);
suppressed_artifact_hint_preservation_test!(
    artifact_hint_404_preserves_every_success_branch,
    SuppressedArtifactListResponse::Missing
);
suppressed_artifact_hint_preservation_test!(
    artifact_hint_rate_limit_preserves_every_success_branch,
    SuppressedArtifactListResponse::RateLimited
);
suppressed_artifact_hint_preservation_test!(
    artifact_hint_timeout_preserves_every_success_branch,
    SuppressedArtifactListResponse::Timeout
);
suppressed_artifact_hint_preservation_test!(
    artifact_hint_malformed_response_preserves_every_success_branch,
    SuppressedArtifactListResponse::Malformed
);
suppressed_artifact_hint_preservation_test!(
    artifact_hint_generic_provider_error_preserves_every_success_branch,
    SuppressedArtifactListResponse::ProviderError
);

#[derive(Clone, Default)]
struct WarningCapture(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

// Thread-default tracing dispatchers share callsite interest caching. Keep the
// two dispatcher-installing tests serialized so they cannot mask each other's
// warning events when the test harness runs them concurrently.
static TELEMETRY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct FieldCapture(BTreeMap<String, String>);

impl Visit for FieldCapture {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

impl<S: Subscriber> Layer<S> for WarningCapture {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut fields = FieldCapture(BTreeMap::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn direct_job_hint_failure_telemetry_retains_resolved_run_id() {
    let _telemetry_test_guard = TELEMETRY_TEST_LOCK.lock().await;
    let server = MockServer::start().await;
    mount_direct_job_detail(&server).await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{OWNER}/{REPO}/actions/runs/123/artifacts"
        )))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let telemetry = WarningCapture::default();
    let subscriber = tracing_subscriber::registry().with(telemetry.clone());
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    let output = append_artifact_hint(
        &mock_client(&server),
        OWNER,
        REPO,
        "task-telemetry",
        Some(900),
        None,
        "direct job log".to_string(),
    )
    .await;

    assert_eq!(output, "direct job log");
    let events = telemetry.0.lock().unwrap();
    let warning = events
        .iter()
        .find(|fields| {
            fields.get("operation").map(String::as_str) == Some("ci_job_log_artifact_hint")
        })
        .expect("suppressed artifact-list failure emits telemetry");
    assert_eq!(
        warning.get("outcome").map(String::as_str),
        Some("suppressed_provider_error")
    );
    assert_eq!(warning.get("job_id").map(String::as_str), Some("Some(900)"));
    assert_eq!(warning.get("run_id").map(String::as_str), Some("Some(123)"));
}

#[tokio::test(flavor = "current_thread")]
async fn direct_job_hint_timeout_telemetry_retains_resolved_run_id() {
    let _telemetry_test_guard = TELEMETRY_TEST_LOCK.lock().await;
    let server = MockServer::start().await;
    mount_direct_job_detail(&server).await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{OWNER}/{REPO}/actions/runs/123/artifacts"
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(ARTIFACT_HINT_TIMEOUT + Duration::from_millis(100)),
        )
        .mount(&server)
        .await;
    let telemetry = WarningCapture::default();
    let subscriber = tracing_subscriber::registry().with(telemetry.clone());
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();

    let output = append_artifact_hint(
        &mock_client(&server),
        OWNER,
        REPO,
        "task-timeout-telemetry",
        Some(900),
        None,
        "direct job log".to_string(),
    )
    .await;

    assert_eq!(output, "direct job log");
    let events = telemetry.0.lock().unwrap();
    let warning = events
        .iter()
        .find(|fields| {
            fields.get("operation").map(String::as_str) == Some("ci_job_log_artifact_hint")
        })
        .expect("suppressed artifact-list timeout emits telemetry");
    assert_eq!(
        warning.get("outcome").map(String::as_str),
        Some("suppressed_timeout")
    );
    assert_eq!(warning.get("job_id").map(String::as_str), Some("Some(900)"));
    assert_eq!(warning.get("run_id").map(String::as_str), Some("Some(123)"));
}

#[tokio::test]
async fn artifact_hint_uses_direct_job_run_and_preserves_artifact_order() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/actions/jobs/900")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 900, "run_id": 123, "name": "tests", "status": "completed",
            "conclusion": "failure", "html_url": "https://example.test/jobs/900"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{OWNER}/{REPO}/actions/runs/123/artifacts"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 2,
            "artifacts": [
                {"id": 1, "name": "first.zip", "size_in_bytes": 1, "expired": false},
                {"id": 2, "name": "second.zip", "size_in_bytes": 2, "expired": false}
            ]
        })))
        .mount(&server)
        .await;
    let output = append_artifact_hint(
        &mock_client(&server),
        OWNER,
        REPO,
        "task",
        Some(900),
        None,
        "existing cleaned log".to_string(),
    )
    .await;
    assert!(output.starts_with("existing cleaned log\n\nWorkflow run 123"));
    assert!(output.contains("`first.zip`, `second.zip`"));
    assert!(output.contains("ci_artifact(action=\"list\", run_id=123)"));
    assert!(output.contains("ci_artifact(action=\"fetch\", run_id=123, artifact=\"first.zip\")"));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        2,
        "direct mode gets one detail and one list"
    );
}

async fn mount_runs(server: &MockServer, query: (&str, &str), runs: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/actions/runs")))
        .and(query_param(query.0, query.1))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"workflow_runs": runs})),
        )
        .mount(server)
        .await;
}

#[tokio::test]
async fn resolver_pr_head_failure_precedes_recorded_and_live_merge_queue() {
    let server = MockServer::start().await;
    mount_runs(
        &server,
        ("head_sha", "recorded-sha"),
        serde_json::json!([run_json(101, Some("failure"), Some("feature"))]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/actions/runs/101/jobs")))
        .respond_with(ResponseTemplate::new(200).set_body_json(jobs_json("failure")))
        .mount(&server)
        .await;
    let resolved = resolve_workflow_run(
        &mock_client(&server),
        OWNER,
        REPO,
        WorkflowRunResolutionRequest {
            pr_number: Some(42),
            recorded_head_sha: Some("recorded-sha".into()),
            recorded_merge_queue_run_id: Some(202),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(resolved.run_id, 101);
    assert_eq!(resolved.lane, WorkflowRunLane::PrHead);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        2,
        "recorded/live merge queue must not be queried"
    );
    assert!(requests[0].url.path().ends_with("/actions/runs"));
    assert!(requests[1].url.path().ends_with("/actions/runs/101/jobs"));
}

#[tokio::test]
async fn resolver_live_merge_group_requires_failing_job_verification() {
    let server = MockServer::start().await;
    mount_runs(&server, ("head_sha", "head"), serde_json::json!([])).await;
    mount_runs(
        &server,
        ("event", "merge_group"),
        serde_json::json!([run_json(
            303,
            Some("failure"),
            Some("gh-readonly-queue/main/pr-42-deadbeef")
        )]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/actions/runs/303/jobs")))
        .respond_with(ResponseTemplate::new(200).set_body_json(jobs_json("failure")))
        .mount(&server)
        .await;
    let resolved = resolve_workflow_run(
        &mock_client(&server),
        OWNER,
        REPO,
        WorkflowRunResolutionRequest {
            pr_number: Some(42),
            recorded_head_sha: Some("head".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(resolved.lane, WorkflowRunLane::LiveMergeGroup);
    let paths = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.url.path().to_owned())
        .collect::<Vec<_>>();
    assert!(paths[2].ends_with("/actions/runs/303/jobs"));
}

#[tokio::test]
async fn resolver_nonfailing_candidates_return_final_no_failure_error() {
    let server = MockServer::start().await;
    mount_runs(
        &server,
        ("head_sha", "head"),
        serde_json::json!([run_json(1, Some("success"), None), run_json(2, None, None)]),
    )
    .await;
    mount_runs(
        &server,
        ("event", "merge_group"),
        serde_json::json!([
            run_json(3, Some("success"), Some("gh-readonly-queue/main/pr-42-x")),
            run_json(4, None, Some("gh-readonly-queue/main/pr-42-y"))
        ]),
    )
    .await;
    let error = resolve_workflow_run(
        &mock_client(&server),
        OWNER,
        REPO,
        WorkflowRunResolutionRequest {
            pr_number: Some(42),
            recorded_head_sha: Some("head".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert!(error.contains("no failing workflow run"));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}
