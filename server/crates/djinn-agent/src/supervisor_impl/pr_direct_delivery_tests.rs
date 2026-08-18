//! The pod-worker task-PR-open path's direct-delivery gate.
//!
//! This is the *other* production PR-open body — the one a worker Pod reaches:
//! `src/server/state/mod.rs` builds the RPC `SupervisorServices` from
//! [`crate::supervisor::services_for_agent_context_with_build_lease`],
//! `djinn-supervisor` calls `open_pr` on it, and
//! [`crate::direct_services::DirectServices::open_pr`] lands in
//! [`super::pr::supervisor_pr_open`].
//!
//! Until this module landed it had no direct-delivery eligibility check and no
//! boundary recorder at all: it created a real GitHub PR and wrote `pr_url` for
//! any task, canonically direct-owned or not. That is what made the ledger
//! SQL's `pr_url IS NULL` legacy discriminator unsafe — the row it claimed
//! production could never mint was minted right here.
//!
//! The three tests below cover the three things that matters:
//!
//! 1. the gate refuses a direct identity over a **real RPC round trip**, so it
//!    is on the path production actually takes, not just on a function;
//! 2. no task-PR forge boundary is reached for a direct identity;
//! 3. a retained-legacy row still opens its PR *and* records that forge
//!    boundary — the positive control that makes (2) an observation rather than
//!    an absence, and that proves the recorder on this path is live.

use std::sync::Arc;

use djinn_db::{Database, TaskRepository};
use djinn_runtime::{SupervisorFlow, TaskRunOutcome, TaskRunSpec};
use djinn_supervisor::SupervisorServices;
use tokio_util::sync::CancellationToken;

use djinn_coordinator::direct_delivery::{BoundaryOperation, boundary_operations_scope};

const INSTALLATION: u64 = 74_242;
const PR_URL: &str = "https://github.com/acme/widget/pull/91";
const HEAD: &str = "2222222222222222222222222222222222222222";

/// Every task-PR forge boundary a direct identity must never reach from this
/// path. Kept as an explicit list rather than "any operation" so a newly
/// recorded boundary has to be classified deliberately.
const TASK_PR_FORGE_OPERATIONS: [BoundaryOperation; 5] = [
    BoundaryOperation::SupervisorPrOpen,
    BoundaryOperation::TaskPrLookup,
    BoundaryOperation::TaskPrAdopt,
    BoundaryOperation::TaskPrCreate,
    BoundaryOperation::TaskPrMerge,
];

/// The retained-legacy fixture mutates process-wide GitHub App configuration
/// and the push transport override, so its two users serialize.
static LEGACY_LIFECYCLE_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    db: Database,
    tasks: TaskRepository,
    task: djinn_core::models::Task,
}

/// Persist a project, an epic, and an `approved` task the PR-open body would
/// legitimately be handed.
async fn build_fixture() -> Fixture {
    let db = crate::test_helpers::create_test_db();
    let project = crate::test_helpers::create_test_project(&db).await;
    let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
    let tasks = TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
    // `approved` is the status the supervisor stage loop hands the PR-open body.
    let task = tasks
        .create(
            &epic.id,
            "pod worker source",
            "",
            "",
            "task",
            0,
            "",
            Some("approved"),
        )
        .await
        .unwrap();
    djinn_db::test_support::persist_project_github_installation_for_test(
        &db,
        &project.id,
        "acme",
        "widget",
        INSTALLATION,
    )
    .await;
    assert!(
        task.pr_url.is_none(),
        "routing must never have nullable PR data to infer from"
    );
    Fixture { db, tasks, task }
}

/// Give the fixture task a canonical direct owner: an active epoch, an active
/// build attempt reachable from its epic's proposal, and a mid-flight
/// generation. Nothing here writes a task field the gate could read directly.
async fn own_directly(fixture: &Fixture) {
    djinn_db::test_support::seed_direct_delivery_liveness_fixture_for_test(
        &fixture.db,
        fixture.task.epic_id.as_deref().expect("fixture epic"),
        &fixture.task.id,
        Some("applying"),
    )
    .await;
}

fn spec_for(fixture: &Fixture, task_branch: &str) -> TaskRunSpec {
    TaskRunSpec {
        task_run_id: "pod-worker-run".into(),
        task_attempt_id: None,
        task_id: fixture.task.id.clone(),
        execution_generation: 0,
        project_id: fixture.task.project_id.clone(),
        trigger: djinn_core::models::TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: task_branch.into(),
        flow: SupervisorFlow::NewTask,
        model_id_per_role: std::collections::HashMap::new(),
        read_source_project_ids: vec![],
        knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
    }
}

/// AC1 — the gate is on the path production takes, not merely on the function.
///
/// The whole pod-worker chain runs: a `SupervisorServices` built by the same
/// constructor `state/mod.rs` uses, served over a real unix-socket RPC server,
/// called through `RpcServices` exactly as `djinn-supervisor` calls it. The
/// direct identity has to be refused across that wire, and no `pr_url` may be
/// written.
///
/// Deleting the eligibility match at the head of `supervisor_pr_open` makes
/// this red: the body then runs on to the GitHub App check and returns
/// `Failed`, not `Escalated`.
#[tokio::test]
async fn pod_worker_rpc_pr_open_refuses_a_direct_identity() {
    let fixture = build_fixture().await;
    own_directly(&fixture).await;

    let host = crate::supervisor::services_for_agent_context_with_build_lease(
        crate::test_helpers::agent_context_from_db(fixture.db.clone(), CancellationToken::new()),
        CancellationToken::new(),
        Arc::new(djinn_coordinator::build_lease::BuildLeaseService::new(
            Arc::new(djinn_db::BuildLeaseRepository::new(fixture.db.clone())),
            0,
        )),
    );
    // Unix-domain socket paths cap out around 108 bytes; the standard test temp
    // root nests under Cargo's target directory and blows that on CI checkouts.
    let dir = tempfile::Builder::new()
        .prefix("dj-pr-gate-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let socket = dir.path().join("supervisor.sock");
    let _server = djinn_supervisor::serve_on_unix_socket(&socket, host)
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let (rpc, _background) = djinn_supervisor::RpcServices::connect_unix(
        &socket,
        cancel.clone(),
        djinn_core::cancel_origin::CancelOriginTag::new(),
    )
    .await
    .unwrap();

    let outcome = rpc
        .open_pr(&spec_for(&fixture, "task/pod-worker"), &fixture.task)
        .await;

    assert!(
        matches!(outcome, TaskRunOutcome::Escalated { .. }),
        "the pod-worker RPC path must refuse a direct identity, got {outcome:?}"
    );
    assert_eq!(
        fixture
            .tasks
            .get(&fixture.task.id)
            .await
            .unwrap()
            .unwrap()
            .pr_url,
        None,
        "a refused direct identity must not acquire a task-PR identity"
    );
    cancel.cancel();
}

/// AC2 (negative half) — the recorder covers this path, and a direct identity
/// reaches none of its task-PR forge boundaries.
///
/// Called in-process rather than over RPC because the boundary recorder is
/// owned by the calling thread; the RPC server answers on another. The body is
/// the identical one the RPC dispatch above reaches
/// (`DirectServices::open_pr` → `supervisor_pr_open`).
#[tokio::test]
async fn pod_worker_pr_open_reaches_no_task_pr_forge_boundary_for_a_direct_identity() {
    let boundary = boundary_operations_scope().await;
    let fixture = build_fixture().await;
    own_directly(&fixture).await;

    let services = crate::direct_services::DirectServices::new(
        crate::test_helpers::agent_context_from_db(fixture.db.clone(), CancellationToken::new()),
        CancellationToken::new(),
    );
    let checkpoint = boundary.checkpoint();
    let outcome = services
        .open_pr(&spec_for(&fixture, "task/pod-worker"), &fixture.task)
        .await;
    let operations = boundary.operations_since(checkpoint);

    assert!(
        matches!(outcome, TaskRunOutcome::Escalated { .. }),
        "expected Escalated, got {outcome:?}"
    );
    let reached: Vec<_> = operations
        .iter()
        .filter(|op| TASK_PR_FORGE_OPERATIONS.contains(op))
        .collect();
    assert!(
        reached.is_empty(),
        "a direct identity reached task-PR forge boundaries from the pod-worker path: {reached:?} (all: {operations:?})"
    );
    // The gate is not silent about *why*: it read the canonical contract.
    assert!(
        operations.contains(&BoundaryOperation::ResolveTaskActiveAttempt),
        "the gate must resolve canonical ownership rather than read a task field: {operations:?}"
    );
    assert_eq!(
        fixture
            .tasks
            .get(&fixture.task.id)
            .await
            .unwrap()
            .unwrap()
            .pr_url,
        None
    );
}

/// AC2 (positive control) — the same recorder on the same path *does* fire for
/// a retained-legacy row, which also proves the gate does not block legacy.
///
/// This pays for a real bare git repository, a real mirror push, and a real
/// installation-authenticated GitHub client against a local HTTP double, so the
/// absence asserted above is an observation and not a fixture that never got
/// far enough to record anything. Removing the two
/// `observe_boundary_operation` calls in front of the provider lookup makes
/// this red.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pod_worker_pr_open_records_its_forge_boundary_for_a_retained_legacy_row() {
    let _guard = LEGACY_LIFECYCLE_GUARD.lock().await;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    async fn git(dir: &std::path::Path, args: &[&str]) {
        let status = djinn_git::git_command()
            .args(args)
            .current_dir(dir)
            .status()
            .await
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    let server = MockServer::start().await;
    let pr = serde_json::json!({"number":91,"title":"retained","state":"open","merged":false,"html_url":PR_URL,"head":{"ref":"task/retained","sha":HEAD},"base":{"ref":"main","sha":"base"},"node_id":"PR_retained"});
    Mock::given(method("GET"))
        .and(path("/repos/acme/widget/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([pr])))
        .mount(&server)
        .await;

    let fixture = build_fixture().await;
    // An active epoch with a canonical owner, but the task carries the explicit
    // legacy label — the one discriminator that keeps it on the task-PR route.
    own_directly(&fixture).await;
    fixture
        .tasks
        .update_labels(
            &fixture.task.id,
            &format!(r#"["{}"]"#, djinn_db::LEGACY_DELIVERY_LABEL),
        )
        .await
        .unwrap();
    djinn_provider::github_app::installations::prime_cache_for_tests(
        INSTALLATION,
        "ghs_pod_worker_fixture",
    );

    let root = crate::test_helpers::test_tempdir("pod-worker-legacy-git-");
    let mirror_root = root.path().join("mirrors");
    std::fs::create_dir_all(&mirror_root).unwrap();
    let bare = mirror_root.join(format!("{}.git", fixture.task.project_id));
    git(root.path(), &["init", "--bare", bare.to_str().unwrap()]).await;
    let work = root.path().join("work");
    git(
        root.path(),
        &["clone", bare.to_str().unwrap(), work.to_str().unwrap()],
    )
    .await;
    git(&work, &["config", "user.email", "fixture@test"]).await;
    git(&work, &["config", "user.name", "fixture"]).await;
    git(&work, &["checkout", "-b", "main"]).await;
    git(&work, &["commit", "--allow-empty", "-m", "base"]).await;
    git(&work, &["push", "origin", "main"]).await;
    git(&work, &["checkout", "-b", "task/retained"]).await;
    git(&work, &["commit", "--allow-empty", "-m", "work"]).await;
    git(&work, &["push", "origin", "task/retained"]).await;

    unsafe { std::env::set_var("GITHUB_APP_ID", "1") };
    super::pr::set_github_base_url_override_for_test(Some(server.uri()));
    super::pr::set_push_url_override_for_test(Some(format!("file://{}", bare.display())));
    let mut context =
        crate::test_helpers::agent_context_from_db(fixture.db.clone(), CancellationToken::new());
    context.mirror = Some(Arc::new(djinn_workspace::MirrorManager::new(mirror_root)));
    let services = crate::direct_services::DirectServices::new(context, CancellationToken::new());

    let boundary = boundary_operations_scope().await;
    let checkpoint = boundary.checkpoint();
    let outcome = services
        .open_pr(&spec_for(&fixture, "task/retained"), &fixture.task)
        .await;
    let operations = boundary.operations_since(checkpoint);

    super::pr::set_push_url_override_for_test(None);
    super::pr::set_github_base_url_override_for_test(None);

    assert!(
        matches!(outcome, TaskRunOutcome::PrOpened { ref url, .. } if url == PR_URL),
        "the retained-legacy row must still open its PR, got {outcome:?}"
    );
    assert_eq!(
        fixture
            .tasks
            .get(&fixture.task.id)
            .await
            .unwrap()
            .unwrap()
            .pr_url
            .as_deref(),
        Some(PR_URL)
    );
    for expected in [
        BoundaryOperation::SupervisorPrOpen,
        BoundaryOperation::TaskPrLookup,
        BoundaryOperation::TaskPrAdopt,
    ] {
        assert!(
            operations.contains(&expected),
            "the pod-worker path must record {expected:?} at its forge boundary: {operations:?}"
        );
    }
}
