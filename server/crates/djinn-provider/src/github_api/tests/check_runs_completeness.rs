//! The check-run enumeration reports its own completeness (proposal `nafu`).
//!
//! Before this verdict existed, a non-success page warned, broke, and returned
//! the prefix it had collected as if it were the whole set. Two silent failure
//! modes followed:
//!
//! 1. a partial set whose fetched members all passed read as **green**, and
//! 2. a *first*-page failure read as an **authoritatively empty** set, because
//!    nothing was ever parsed so `total_count` stayed `0` and the obvious
//!    `collected < total_count` guard compared equal.
//!
//! (2) is the reason these tests exist as their own module rather than as one
//! more assertion on the pagination test: the arithmetic guard alone is not a
//! completeness proof, and the fixture below is the counterexample.

use super::seed_installation_token;
use crate::github_api::{
    CheckRunsResponse, CheckSetCompleteness, CheckSetIncompleteReason, GitHubApiClient,
};
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The PR body every fixture here shares. Head SHA `abc123` is what the
/// check-runs path is keyed on.
fn pr_body() -> serde_json::Value {
    serde_json::json!({
        "number": 42,
        "title": "feat: add feature",
        "state": "open",
        "merged": false,
        "html_url": "https://github.com/djinnos/server/pull/42",
        "head": { "ref": "feature-branch", "sha": "abc123" },
        "base": { "ref": "main", "sha": "def456" },
        "auto_merge": null,
        "node_id": "PR_abc123"
    })
}

fn check_run(id: u64) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "name": format!("check-{id}"),
        "status": "completed",
        "conclusion": "success",
        "html_url": format!("https://github.com/djinnos/server/actions/runs/{id}/job/{id}")
    })
}

fn page(total_count: u32, ids: std::ops::Range<u64>) -> serde_json::Value {
    serde_json::json!({
        "total_count": total_count,
        "check_runs": ids.map(check_run).collect::<Vec<_>>(),
    })
}

async fn mount_pr(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/repos/djinnos/server/pulls/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr_body()))
        .mount(server)
        .await;
}

const CHECK_RUNS_PATH: &str = r"/repos/djinnos/server/commits/abc123/check-runs";

/// A page-1 failure is the case the `total_count` comparison cannot see.
///
/// Nothing is parsed, so `total_count` stays `0` and zero runs are collected —
/// `0 < 0` is false. Without the failed-page fact carried explicitly, this is
/// indistinguishable from "this repository has no CI configured", which is a
/// lane's *fast path to green*. The verdict must say `PageFetchFailed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_first_page_is_incomplete_not_empty() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    mount_pr(&server).await;

    Mock::given(method("GET"))
        .and(path_regex(CHECK_RUNS_PATH))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream is unwell"))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let (_pr, checks): (_, CheckRunsResponse) = client
        .get_pull_request("djinnos", "server", 42)
        .await
        .unwrap();

    // The shape that used to read as "no CI configured".
    assert!(checks.check_runs.is_empty());
    assert_eq!(checks.total_count, 0);
    assert_eq!(
        checks.check_runs.len(),
        checks.total_count as usize,
        "precondition: the count comparison agrees, which is exactly why it \
         cannot be the completeness proof",
    );

    assert_eq!(
        checks.completeness,
        CheckSetCompleteness::Incomplete(CheckSetIncompleteReason::PageFetchFailed),
    );
    assert!(!checks.completeness.is_complete());
}

/// A later-page failure returns a non-empty *prefix*. Every member of that
/// prefix here is `success`, so a consumer that trusted the collected list
/// would record `Passing` for a set that may contain a causal failure it never
/// saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_later_page_is_incomplete() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    mount_pr(&server).await;

    // A full first page forces the loop on to page 2.
    Mock::given(method("GET"))
        .and(path_regex(CHECK_RUNS_PATH))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(150, 0..100)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(CHECK_RUNS_PATH))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let (_pr, checks): (_, CheckRunsResponse) = client
        .get_pull_request("djinnos", "server", 42)
        .await
        .unwrap();

    assert_eq!(checks.check_runs.len(), 100, "the prefix is still returned");
    assert!(
        checks
            .check_runs
            .iter()
            .all(|cr| cr.conclusion.as_deref() == Some("success")),
        "precondition: every fetched member passed — a trusting consumer would \
         call this green",
    );
    assert_eq!(
        checks.completeness,
        CheckSetCompleteness::Incomplete(CheckSetIncompleteReason::PageFetchFailed),
    );
}

/// Two distinct arithmetic-adjacent facts, kept apart on purpose.
///
/// * **Truncation:** every page succeeded and the walk was cut off by
///   `MAX_PAGES` with a full final page. Here `total_count` and the collected
///   count *agree* at 1000, so only the ceiling fact detects it.
/// * **Short read:** every page succeeded, the walk ended naturally on a short
///   page, and the provider's own `total_count` is larger than what arrived.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncation_and_short_read_are_incomplete() {
    // --- MAX_PAGES truncation ---------------------------------------------
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    mount_pr(&server).await;

    // 10 full pages of 100 and a `total_count` of 1500: the walk was cut off by
    // `MAX_PAGES` with 500 runs still unseen. This is genuine truncation.
    for p in 1..=10u64 {
        let start = (p - 1) * 100;
        Mock::given(method("GET"))
            .and(path_regex(CHECK_RUNS_PATH))
            .and(query_param("page", p.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(1500, start..start + 100)))
            .mount(&server)
            .await;
    }

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let (_pr, checks): (_, CheckRunsResponse) = client
        .get_pull_request("djinnos", "server", 42)
        .await
        .unwrap();

    assert_eq!(checks.check_runs.len(), 1000);
    assert_eq!(checks.total_count, 1500);
    assert_eq!(
        checks.completeness,
        CheckSetCompleteness::Incomplete(CheckSetIncompleteReason::MaxPagesTruncated),
    );

    // --- short read --------------------------------------------------------
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    mount_pr(&server).await;

    // One short page that claims 7 but delivers 3.
    Mock::given(method("GET"))
        .and(path_regex(CHECK_RUNS_PATH))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(7, 0..3)))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let (_pr, checks): (_, CheckRunsResponse) = client
        .get_pull_request("djinnos", "server", 42)
        .await
        .unwrap();

    assert_eq!(checks.check_runs.len(), 3);
    assert_eq!(checks.total_count, 7);
    assert_eq!(
        checks.completeness,
        CheckSetCompleteness::Incomplete(CheckSetIncompleteReason::ShortRead),
    );
}

/// The positive side of the verdict: a complete single page, a complete
/// multi-page walk that ends on a short final page, and a genuinely empty
/// set — the last of which is the *authoritatively* complete-empty enumeration
/// the two lane compatibility paths are allowed to act on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_single_and_final_pages_are_complete() {
    // --- complete single page ---------------------------------------------
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    mount_pr(&server).await;
    Mock::given(method("GET"))
        .and(path_regex(CHECK_RUNS_PATH))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(3, 0..3)))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let (_pr, checks): (_, CheckRunsResponse) = client
        .get_pull_request("djinnos", "server", 42)
        .await
        .unwrap();
    assert_eq!(checks.completeness, CheckSetCompleteness::Complete);
    assert!(checks.completeness.is_complete());

    // --- complete walk ending on a short final page ------------------------
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    mount_pr(&server).await;
    Mock::given(method("GET"))
        .and(path_regex(CHECK_RUNS_PATH))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(142, 0..100)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(CHECK_RUNS_PATH))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(142, 100..142)))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let (_pr, checks): (_, CheckRunsResponse) = client
        .get_pull_request("djinnos", "server", 42)
        .await
        .unwrap();
    assert_eq!(checks.check_runs.len(), 142);
    assert_eq!(checks.completeness, CheckSetCompleteness::Complete);

    // --- authoritatively complete EMPTY ------------------------------------
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    mount_pr(&server).await;
    Mock::given(method("GET"))
        .and(path_regex(CHECK_RUNS_PATH))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(0, 0..0)))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let (_pr, checks): (_, CheckRunsResponse) = client
        .get_pull_request("djinnos", "server", 42)
        .await
        .unwrap();
    assert!(checks.check_runs.is_empty());
    assert_eq!(
        checks.completeness,
        CheckSetCompleteness::Complete,
        "an empty set that was actually fetched is complete — this is the one \
         empty set the lane no-CI paths may act on, and it is byte-identical \
         in shape to the failed-first-page fixture above",
    );
}

/// Reaching the ceiling is not the same fact as being truncated by it.
///
/// A PR with exactly `MAX_PAGES * PER_PAGE` check runs produces a *full* tenth
/// page, which sets the ceiling flag — and it also produces a `total_count` that
/// exactly matches what arrived, which proves nothing was left behind. The
/// verdict used to be computed from the flag alone, so such a PR was declared
/// permanently incomplete; and unlike a failed page, `MaxPagesTruncated` has no
/// re-poll that can clear it, so that PR's CI gate wedged for good.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_walk_that_exactly_fills_the_ceiling_is_complete() {
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    mount_pr(&server).await;

    for p in 1..=10u64 {
        let start = (p - 1) * 100;
        Mock::given(method("GET"))
            .and(path_regex(CHECK_RUNS_PATH))
            .and(query_param("page", p.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(page(1000, start..start + 100)))
            .mount(&server)
            .await;
    }

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let (_pr, checks): (_, CheckRunsResponse) = client
        .get_pull_request("djinnos", "server", 42)
        .await
        .unwrap();

    assert_eq!(checks.check_runs.len(), 1000);
    assert_eq!(
        checks.check_runs.len(),
        checks.total_count as usize,
        "precondition: the provider's own count agrees with what arrived",
    );
    assert_eq!(
        checks.completeness,
        CheckSetCompleteness::Complete,
        "a full final page whose count is exonerated by `total_count` is complete",
    );
}

// ---------------------------------------------------------------------------
// `list_check_runs_for_ref`
// ---------------------------------------------------------------------------

const REF_CHECK_RUNS_PATH: &str = r"/repos/djinnos/server/commits/queuesha/check-runs";

/// The merge-group lane's enumeration used to forge its verdict.
///
/// It fetched a single page of 100 and returned the parsed body unchanged. The
/// `completeness` field is `#[serde(default)]` and the default is `Complete`, so
/// a merge-group ref with more than 100 check runs silently truncated *and told
/// every caller the set was authoritative* — defeating the exact contract the
/// field exists for, on the one path where the real merge-queue failures live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ref_enumeration_pages_and_reports_its_own_completeness() {
    // --- multi-page walk ending on a short final page: complete ------------
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    Mock::given(method("GET"))
        .and(path_regex(REF_CHECK_RUNS_PATH))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(142, 0..100)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(REF_CHECK_RUNS_PATH))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(142, 100..142)))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let checks = client
        .list_check_runs_for_ref("djinnos", "server", "queuesha")
        .await
        .unwrap();
    assert_eq!(
        checks.check_runs.len(),
        142,
        "the second page must be fetched, not silently dropped"
    );
    assert_eq!(checks.completeness, CheckSetCompleteness::Complete);

    // --- short read: incomplete, not a forged Complete ---------------------
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    Mock::given(method("GET"))
        .and(path_regex(REF_CHECK_RUNS_PATH))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(7, 0..3)))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let checks = client
        .list_check_runs_for_ref("djinnos", "server", "queuesha")
        .await
        .unwrap();
    assert_eq!(checks.check_runs.len(), 3);
    assert_eq!(
        checks.completeness,
        CheckSetCompleteness::Incomplete(CheckSetIncompleteReason::ShortRead),
        "a serde default must never stand in for a completeness computation",
    );

    // --- a later page failing is INCOMPLETE, not a truncated Complete ------
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    Mock::given(method("GET"))
        .and(path_regex(REF_CHECK_RUNS_PATH))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(250, 0..100)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex(REF_CHECK_RUNS_PATH))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(502).set_body_string("bad gateway"))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    let checks = client
        .list_check_runs_for_ref("djinnos", "server", "queuesha")
        .await
        .unwrap();
    assert_eq!(checks.check_runs.len(), 100);
    assert_eq!(
        checks.completeness,
        CheckSetCompleteness::Incomplete(CheckSetIncompleteReason::PageFetchFailed),
    );

    // --- a page-1 failure still surfaces as an error ----------------------
    //
    // There is no partial result to report, and the merge-group route treats an
    // outright failure as `CheckApiError` — a complete-but-unusable input whose
    // run identity is already known — rather than as a re-pollable enumeration
    // failure.
    let server = MockServer::start().await;
    let install_id = seed_installation_token();
    Mock::given(method("GET"))
        .and(path_regex(REF_CHECK_RUNS_PATH))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&server)
        .await;

    let client = GitHubApiClient::for_installation_with_base_url(install_id, server.uri());
    assert!(
        client
            .list_check_runs_for_ref("djinnos", "server", "queuesha")
            .await
            .is_err(),
        "a page-1 failure has no partial result and must not read as an empty set",
    );
}
