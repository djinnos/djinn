//! Tests for the supervisor PR-open path.
//!
//! Focused unit tests for the unchanged-head red-CI remediation rejection
//! guard. The pure predicate [`unchanged_head_rejection_reason`] is tested
//! directly — it does not require a database.
//!
//! m116 / `llvt` — also tests the publication-failure / mirror-divergence
//! short-circuit ([`super::unpublished_mirror_publication_reason`]) that
//! suppresses the false-strike unchanged-head rejection when the mirror
//! produced a commit but GitHub never received it.

use super::{
    UnchangedHeadContext, unchanged_head_rejection_reason, unpublished_mirror_publication_reason,
};

/// The retained-legacy lifecycle fixtures deliberately exercise test-only
/// process-global transport overrides and the shared boundary recorder. Hold
/// this guard for each full lifecycle so their real supervisor/poller effects
/// cannot clear or redirect one another when the test runtime runs in parallel.
static RETAINED_LEGACY_LIFECYCLE_GUARD: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

// ── Unchanged-head remediation rejection predicate ──────────────────────────

/// When `ci_last_remediation_base_sha` matches the post-session head SHA, the
/// predicate returns `Some(reason)` — the submit must be rejected.
#[test]
fn unchanged_head_rejects_when_sha_matches_baseline() {
    let base_sha = "abc123def456789012345678901234567890abcd";
    let reason = unchanged_head_rejection_reason(
        Some(base_sha),
        base_sha,
        "task-uuid-1234",
        "itmo",
        Some(42),
    );

    let reason = reason.expect("unchanged head SHA must produce a rejection reason");

    // The reason must explain that no new commit was produced.
    assert!(
        reason.contains("unchanged"),
        "reason must mention 'unchanged': {reason}"
    );
    assert!(
        reason.to_lowercase().contains("no new commit was produced"),
        "reason must explain no new commit was produced: {reason}"
    );
    assert!(
        reason.contains("remediation"),
        "reason must mention remediation: {reason}"
    );
    // The unchanged head SHA must be present.
    assert!(
        reason.contains(base_sha),
        "reason must contain the unchanged head SHA: {reason}"
    );
    // The PR number must be present.
    assert!(
        reason.contains("PR #42"),
        "reason must contain the PR number: {reason}"
    );
    // The task short_id must be present.
    assert!(
        reason.contains("itmo"),
        "reason must contain the task short_id: {reason}"
    );
}

/// When the head SHA changed from the baseline, the predicate returns `None`
/// — the submit is NOT rejected and proceeds through the normal PR-open path.
#[test]
fn changed_head_does_not_take_rejection_path() {
    let base_sha = "abc123def456789012345678901234567890abcd";
    let new_sha = "fedcba9876543210fedcba9876543210fedcba98";

    let result = unchanged_head_rejection_reason(
        Some(base_sha),
        new_sha,
        "task-uuid-1234",
        "itmo",
        Some(42),
    );

    assert!(
        result.is_none(),
        "a changed head SHA must NOT produce a rejection reason"
    );
}

/// When no remediation baseline is active (`ci_last_remediation_base_sha` is
/// `None`), the predicate returns `None` — no rejection, regardless of the head
/// SHA. This is the common case: tasks that have never failed required CI.
#[test]
fn no_baseline_does_not_reject() {
    let head_sha = "abc123def456789012345678901234567890abcd";

    let result =
        unchanged_head_rejection_reason(None, head_sha, "task-uuid-1234", "itmo", Some(42));

    assert!(
        result.is_none(),
        "no remediation baseline must not produce a rejection"
    );
}

/// When the PR number is `None` (no snapshot PR number available), the
/// predicate still rejects — the reason message should contain the None PR
/// number representation but still carry the core fields.
#[test]
fn unchanged_head_rejects_without_pr_number() {
    let base_sha = "abc123def456789012345678901234567890abcd";
    let reason =
        unchanged_head_rejection_reason(Some(base_sha), base_sha, "task-uuid-1234", "itmo", None);

    assert!(
        reason.is_some(),
        "unchanged head must still reject even without a PR number"
    );

    let reason = reason.unwrap();
    assert!(
        reason.contains(base_sha),
        "reason must contain the unchanged head SHA: {reason}"
    );
}

/// AC1: The blocking system event payload structure must carry all required
/// fields: task_id, PR number, unchanged head SHA, remediation base SHA,
/// and the blocking reason. Verify the predicate produces a reason string
/// that includes every field the system event would emit.
#[test]
fn unchanged_head_rejection_includes_all_event_fields() {
    let base_sha = "abc123def456789012345678901234567890abcd";
    let task_id = "task-uuid-e2e-1234";
    let short_id = "w396";
    let pr_number = Some(42);

    let reason =
        unchanged_head_rejection_reason(Some(base_sha), base_sha, task_id, short_id, pr_number)
            .expect("unchanged head must produce a rejection reason");

    // The system event payload (emitted by check_unchanged_remediation_head)
    // includes: task_id, short_id, pr_number, head_sha, remediation_base_sha.
    // The reason string must carry the same information for human readability.
    assert!(
        reason.contains(base_sha),
        "must carry the unchanged head SHA"
    );
    assert!(
        reason.contains("PR #42"),
        "must carry the PR number from the event"
    );
    assert!(
        reason.contains(short_id),
        "must carry the task short_id from the event"
    );

    // The blocking reason must explain why the submit was rejected.
    assert!(
        reason.contains("unchanged"),
        "must state the head is unchanged"
    );
    assert!(
        reason.to_lowercase().contains("no new commit was produced"),
        "must explain that no new commit was produced"
    );
    assert!(
        reason.contains("remediation"),
        "must mention the task remains in remediation"
    );
}

/// AC1: The unchanged-head rejection preserves remediation state. When the
/// submit is rejected, the task must remain in remediation — the predicate
/// returns Some (indicating rejection) so the caller can keep the task parked.
/// The predicate must NOT return None (which would allow the submit to proceed).
#[test]
fn unchanged_head_preserves_remediation_state() {
    let base_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    // The task is in remediation: ci_last_remediation_base_sha is set.
    // The submit pushes the same head SHA → must reject.
    let result = unchanged_head_rejection_reason(
        Some(base_sha),
        base_sha,
        "task-uuid-remediation",
        "itmo",
        Some(7),
    );

    // Returning Some means "reject the submit and keep the task in remediation."
    assert!(
        result.is_some(),
        "unchanged head must reject and preserve remediation state"
    );

    // The reason must not suggest the task is advancing.
    let reason = result.unwrap();
    assert!(
        !reason.to_lowercase().contains("advancing")
            && !reason.to_lowercase().contains("proceeding"),
        "rejection reason must not suggest the task is advancing: {reason}"
    );
    assert!(
        reason.contains("remains in remediation"),
        "reason must explicitly state the task remains in remediation: {reason}"
    );
}

/// The rejection reason is deterministic for the same inputs — calling the
/// predicate twice with identical args produces the same reason string.
#[test]
fn rejection_reason_is_deterministic() {
    let base_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    let reason1 = unchanged_head_rejection_reason(
        Some(base_sha),
        base_sha,
        "task-uuid-9999",
        "w396",
        Some(7),
    );
    let reason2 = unchanged_head_rejection_reason(
        Some(base_sha),
        base_sha,
        "task-uuid-9999",
        "w396",
        Some(7),
    );

    assert_eq!(reason1, reason2, "rejection reason must be deterministic");
}

// ── m116 / llvt: publication-failure / mirror-divergence short-circuit predicate ─
//
// These tests verify the m116 behavior that suppresses the misleading
// unchanged-head escalation when mirror↔GitHub reconciliation evidence
// indicates a GitHub publication failure or head divergence. They are pure
// predicate tests (no DB), mirroring the AC#3 invariant:
//   "Coordinator tests cover the publication-failure stale-head false-strike
//    case and at least one non-diverged unchanged-head case that still
//    escalates normally."
//
// The "non-diverged unchanged-head case" is covered above by
// `unchanged_head_rejects_when_sha_matches_baseline` /
// `unchanged_head_preserves_remediation_state` — those remain green, proving
// that the publication-failure path does not swallow legitimate no-progress
// escalations.

/// Build a minimal `UnchangedHeadContext` for the publication-failure tests.
#[allow(clippy::too_many_arguments)]
fn pub_failure_ctx<'a>(
    head_sha: &'a str,
    remediation_base_sha: Option<&'a str>,
    mirror_head_sha: Option<&'a str>,
    github_head_sha: Option<&'a str>,
    heads_diverged: Option<bool>,
    head_observation_error: Option<&'a str>,
    pr_number: Option<i64>,
    short_id: &'a str,
) -> UnchangedHeadContext<'a> {
    UnchangedHeadContext {
        head_sha,
        remediation_base_sha,
        mirror_head_sha,
        github_head_sha,
        heads_diverged,
        head_observation_error,
        pr_number,
        short_id,
    }
}

#[test]
fn pub_failure_returns_none_without_remediation_baseline() {
    // No durable remediation baseline → cannot trigger the guard at all,
    // publication evidence or not.
    let ctx = pub_failure_ctx(
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        None,
        Some("mirror-newer-sha"),
        Some("github-older-sha"),
        Some(true),
        Some("auth failed"),
        Some(7),
        "itmo",
    );

    let result = unpublished_mirror_publication_reason(&ctx);
    assert!(
        result.is_none(),
        "no remediation baseline must not produce a publication-failure reason"
    );
}

#[test]
fn pub_failure_returns_none_when_head_sha_advanced() {
    // Worker DID push a new local mirror head → no publication failure.
    // This proves a legitimate progress round still allows the unchanged-head
    // rejection logic to fall through (it won't fire here either since the
    // head advanced, but the publication predicate must agree).
    let new_sha = "newly-pushed-sha";
    let old_sha = "old-remediation-base";
    let ctx = pub_failure_ctx(
        new_sha,
        Some(old_sha),
        Some(new_sha),
        Some(new_sha),
        Some(false),
        None,
        Some(42),
        "itmo",
    );

    assert!(
        unpublished_mirror_publication_reason(&ctx).is_none(),
        "an advanced head must not produce a publication-failure reason"
    );
}

#[test]
fn pub_failure_suppresses_when_mirror_advanced_and_github_lagging() {
    // The vy47 / aah4 shape:
    //  - remediation baseline = old failing GitHub PR head
    //  - freshly-pushed local head == baseline (push could not reach GitHub)
    //  - latest attempt evidence: mirror advanced past the baseline
    //  - latest attempt evidence: GitHub head == baseline (no advance)
    //  - heads_diverged = true
    // Expectation: predicate returns Some(reason) identifying divergence,
    // NOT "no new commit was produced".
    let baseline = "github-old-sha-failing-baseline";
    let mirror_advanced = "mirror-newly-pushed-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        Some(mirror_advanced),
        Some(baseline),
        Some(true),
        None,
        Some(99),
        "itmo",
    );

    let reason = unpublished_mirror_publication_reason(&ctx)
        .expect("mirror-advanced + github-lagging + divergence flag → publication-failure reason");

    assert!(
        reason.contains("diverged") || reason.contains("divergence"),
        "reason must identify the divergence: {reason}"
    );
    assert!(
        reason.contains(mirror_advanced) || reason.contains("mirror"),
        "reason must reference the mirror head evidence: {reason}"
    );
    assert!(
        reason.contains(baseline),
        "reason must reference the failing remediation baseline: {reason}"
    );
    assert!(
        reason.contains("PR #99"),
        "reason must contain the PR number: {reason}"
    );
    assert!(
        reason.contains("itmo"),
        "reason must contain the task short_id: {reason}"
    );
    // Crucially: this MUST NOT look like a worker-no-commit rejection.
    assert!(
        !reason.contains("no new commit was produced"),
        "publication-failure reason must NOT borrow the no-commit-strike text: {reason}"
    );
    // And it must explicitly say the strike is suppressed.
    assert!(
        reason.contains("NOT") || reason.to_lowercase().contains("suppress"),
        "reason must make clear the unchanged-head strike is suppressed: {reason}"
    );
}

#[test]
fn pub_failure_suppresses_when_publication_error_recorded() {
    // Worker may not have an explicit mirror_head_sha yet, but the latest
    // attempt carried a publication observation error. Together with
    // freshly-pushed local head matching the durable baseline this is a
    // clear publication-failure signal.
    let baseline = "auth-rejected-baseline";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        None,
        Some(baseline),
        None,
        Some("HTTP 403: app installation suspended"),
        Some(123),
        "vy47",
    );

    let reason = unpublished_mirror_publication_reason(&ctx)
        .expect("head_observation_error alone (with github_lagging) must trigger");

    assert!(
        reason.contains("auth-rejected-baseline"),
        "reason must reference the failing remediation baseline: {reason}"
    );
    assert!(
        reason.contains("HTTP 403") || reason.contains("publication"),
        "reason must surface the publication error context: {reason}"
    );
    assert!(
        reason.contains("PR #123"),
        "reason must contain the PR number"
    );
    assert!(
        reason.contains("vy47"),
        "reason must contain the task short_id"
    );
}

#[test]
fn pub_failure_returns_none_when_no_divergence_or_publication_error() {
    // The mirror head == remediation baseline, github head == remediation
    // baseline, and nothing else. This is the *legitimate* unchanged-head
    // case where the worker truly produced no new commit. Publication
    // predicate must NOT swallow this — the unchanged-head rejection must
    // still fire.
    let baseline = "truly-unchanged-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        Some(baseline),
        Some(baseline),
        Some(false),
        None,
        Some(7),
        "itmo",
    );

    assert!(
        unpublished_mirror_publication_reason(&ctx).is_none(),
        "no divergence / publication-error evidence → predicate must NOT suppress"
    );
}

#[test]
fn pub_failure_returns_none_without_active_worker_signal() {
    // Heads_diverged is Some(true) but `mirror_advanced` is false (mirror head
    // equals the baseline coincidentally), and no publication error. The
    // divergent flag alone isn't strong enough — we still need at least one
    // active signal (mirror advanced OR publication-error) to confirm the
    // worker did something GitHub didn't see.
    let baseline = "baseline-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        Some(baseline),
        Some("some-other-github-head"),
        Some(true),
        None,
        Some(8),
        "itmo",
    );

    assert!(
        unpublished_mirror_publication_reason(&ctx).is_none(),
        "divergence flag without active worker signal must NOT trigger suppression"
    );
}

#[test]
fn pub_failure_returns_none_without_divergent_signal() {
    // Mirror advanced, but no divergence flag, no github head, AND no
    // publication error. Without at least one divergent signal
    // (heads_diverged, github_lagging, or pub_error), the predicate must
    // remain conservative and let the unchanged-head rejection fire.
    let baseline = "baseline-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        Some("mirror-newer-sha"),
        None,
        None,
        None,
        Some(8),
        "itmo",
    );

    assert!(
        unpublished_mirror_publication_reason(&ctx).is_none(),
        "without divergence / lagging / pub_error, suppression must not trigger"
    );
}

#[test]
fn pub_failure_combines_mirror_advanced_with_publication_error() {
    // Both signals active (mirror advanced AND a publication error). The
    // reason must surface both pieces of evidence for operators.
    let baseline = "baseline-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        Some("mirror-newer-sha"),
        Some(baseline),
        Some(true),
        Some("TLS handshake failed"),
        Some(11),
        "llvt",
    );

    let reason = unpublished_mirror_publication_reason(&ctx)
        .expect("both signals active → suppression must trigger");

    assert!(
        reason.contains("TLS handshake failed") || reason.contains("publication"),
        "reason must surface the publication error: {reason}"
    );
    assert!(
        reason.contains("mirror") || reason.contains("diverged"),
        "reason must surface the divergence evidence: {reason}"
    );
    assert!(
        !reason.contains("no new commit was produced"),
        "reason must NOT borrow the no-commit-strike text: {reason}"
    );
}

#[test]
fn pub_failure_suppresses_with_observation_error_unknown_github_head_mirror_advanced() {
    // The reviewer-requested regression case: head_observation_error=Some(...)
    // with github_head_sha=None and heads_diverged=None, but the mirror head
    // advanced. Before the fix this returned None (falling through to the
    // misleading unchanged-head rejection) because divergent required
    // explicit_divergence || github_lagging, both of which were false/unknown.
    //
    // Now has_pub_error itself qualifies as a divergent signal, so this must
    // suppress the unchanged-head rejection.
    let baseline = "baseline-sha";
    let mirror_new = "mirror-advanced-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        Some(mirror_new),
        None, // github_head_sha unknown — no PR branch observed
        None, // heads_diverged unknown
        Some("HTTP 403: app installation token revoked"),
        Some(55),
        "llvt",
    );

    let reason = unpublished_mirror_publication_reason(&ctx).expect(
        "observation error + mirror advanced + unknown GitHub head → must suppress unchanged-head",
    );

    assert!(
        reason.contains("publication") || reason.contains("403") || reason.contains("revoked"),
        "reason must surface the publication/observation error: {reason}"
    );
    assert!(
        !reason.contains("no new commit was produced"),
        "reason must NOT borrow the no-commit-strike text: {reason}"
    );
    assert!(
        reason.contains("NOT") || reason.to_lowercase().contains("suppress"),
        "reason must make clear the unchanged-head strike is suppressed: {reason}"
    );
}

#[test]
fn pub_failure_suppresses_with_observation_error_unknown_github_head_mirror_unknown() {
    // Strongest minimal case: head_observation_error=Some(...) is the ONLY
    // signal — github_head_sha=None, heads_diverged=None, mirror_head_sha
    // is also None (no attempt recorded the mirror head yet). Before the fix
    // this returned None; now the publication error alone is sufficient.
    let baseline = "baseline-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        None, // mirror_head_sha unknown
        None, // github_head_sha unknown
        None, // heads_diverged unknown
        Some("GitHub API rate limit exceeded; observation failed"),
        Some(77),
        "itmo",
    );

    let reason = unpublished_mirror_publication_reason(&ctx)
        .expect("observation error alone (all other heads unknown) → must suppress unchanged-head");

    assert!(
        reason.contains("rate limit")
            || reason.contains("publication")
            || reason.contains("observation"),
        "reason must surface the publication/observation error: {reason}"
    );
    assert!(
        !reason.contains("no new commit was produced"),
        "reason must NOT borrow the no-commit-strike text: {reason}"
    );
    assert!(
        reason.contains("NOT") || reason.to_lowercase().contains("suppress"),
        "reason must make clear the unchanged-head strike is suppressed: {reason}"
    );
}

/// The production supervisor entry point must decide eligibility before it can
/// touch its mirror or any GitHub task-PR operation.
#[tokio::test]
async fn supervisor_pr_open_parks_or_excludes_direct_delivery_before_task_pr_effects() {
    use std::collections::HashMap;

    use crate::direct_delivery::{
        BoundaryOperation, LEGACY_DELIVERY_LABEL, clear_boundary_operations,
        take_boundary_operations,
    };
    use crate::supervisor_impl::{SupervisorCallbackContext, supervisor_pr_open};
    use djinn_core::events::EventBus;
    use djinn_core::models::{KnowledgeInjectionConfig, TaskRunTrigger};
    use djinn_db::{
        ActivateProposalBuildAttemptInput, Database, EpicRepository,
        ProposalBuildAttemptRepository, ReserveProposalBuildAttemptInput, TaskRepository,
    };
    use djinn_runtime::{SupervisorFlow, TaskRunOutcome, TaskRunSpec};
    use tokio_util::sync::CancellationToken;

    #[derive(Clone, Copy)]
    enum Fixture {
        Disabled,
        ExplicitLegacy,
        Direct,
        Unresolved,
        MissingContract,
        UnknownContract,
    }

    for fixture in [
        Fixture::Disabled,
        Fixture::ExplicitLegacy,
        Fixture::Direct,
        Fixture::Unresolved,
        Fixture::MissingContract,
        Fixture::UnknownContract,
    ] {
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let epic = EpicRepository::new(db.clone(), events.clone())
            .create("eligibility", "", "", "", "", None)
            .await
            .unwrap();
        let tasks = TaskRepository::new(db.clone(), events);
        let task = tasks
            .create(
                &epic.id,
                "task",
                "",
                "",
                "task",
                0,
                "worker",
                Some("approved"),
            )
            .await
            .unwrap();
        if matches!(fixture, Fixture::ExplicitLegacy) {
            tasks
                .set_pr_url(&task.id, "https://example.test/pr/42")
                .await
                .unwrap();
            tasks
                .update_labels(&task.id, &format!(r#"["{LEGACY_DELIVERY_LABEL}"]"#))
                .await
                .unwrap();
        }
        if !matches!(fixture, Fixture::Disabled) {
            djinn_db::test_support::activate_direct_delivery_epoch_for_test(&db).await;
        }
        if matches!(fixture, Fixture::Direct) {
            djinn_db::test_support::seed_direct_delivery_proposal_owner_for_test(
                &db, &epic.id, "p", "p",
            )
            .await;
            let attempts = ProposalBuildAttemptRepository::new(db.clone());
            attempts
                .reserve(&ReserveProposalBuildAttemptInput {
                    proposal_id: "p".into(),
                    proposal_short_id: "p".into(),
                    build_attempt_id: "a".into(),
                    build_attempt_short_id: "a".into(),
                    observed_base_sha: "base".into(),
                })
                .await
                .unwrap();
            attempts
                .activate(&ActivateProposalBuildAttemptInput {
                    build_attempt_id: "a".into(),
                    expected_lifecycle: djinn_core::models::ProposalBuildAttemptLifecycle::Reserved,
                    expected_branch_head_sha: None,
                    branch_head_sha: "base".into(),
                })
                .await
                .unwrap();
        }
        match fixture {
            Fixture::MissingContract => {
                djinn_db::test_support::remove_direct_delivery_epoch_for_test(&db).await
            }
            Fixture::UnknownContract => {
                djinn_db::test_support::seed_unknown_direct_delivery_epoch_for_test(&db).await
            }
            _ => {}
        }
        let spec = TaskRunSpec {
            task_run_id: "run".into(),
            task_attempt_id: None,
            task_id: task.id.clone(),
            execution_generation: 0,
            project_id: task.project_id.clone(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "task/test".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: vec![],
            knowledge_injection: KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        };
        let callbacks = SupervisorCallbackContext {
            agent_context: crate::test_helpers::coordinator_context_from_db(
                db.clone(),
                CancellationToken::new(),
            ),
            cancel: CancellationToken::new(),
            provider_override: None,
        };
        let task = tasks.get(&task.id).await.unwrap().unwrap();
        clear_boundary_operations();
        let outcome = supervisor_pr_open(&spec, &task, &callbacks).await;
        let operations = take_boundary_operations();
        for forbidden in [
            BoundaryOperation::TaskPrLookup,
            BoundaryOperation::TaskPrAdopt,
            BoundaryOperation::TaskPrCreate,
        ] {
            assert!(
                !operations.contains(&forbidden),
                "direct-delivery boundary reached forbidden task-PR effect {forbidden:?}"
            );
        }
        match fixture {
            Fixture::Direct => {
                assert!(matches!(outcome, TaskRunOutcome::Escalated { .. }));
                assert_eq!(
                    operations,
                    vec![
                        BoundaryOperation::CapabilityProbe,
                        BoundaryOperation::ResolveTaskActiveAttempt
                    ]
                );
            }
            Fixture::Unresolved => {
                assert!(matches!(outcome, TaskRunOutcome::Escalated { .. }));
                assert_eq!(
                    tasks.get(&task.id).await.unwrap().unwrap().status,
                    "needs_lead_intervention"
                );
                assert_eq!(
                    operations,
                    vec![
                        BoundaryOperation::CapabilityProbe,
                        BoundaryOperation::ResolveTaskActiveAttempt,
                        BoundaryOperation::NoProposalOwnerPark
                    ]
                );
            }
            Fixture::MissingContract | Fixture::UnknownContract => {
                assert!(matches!(outcome, TaskRunOutcome::Escalated { .. }));
                assert_eq!(
                    tasks.get(&task.id).await.unwrap().unwrap().status,
                    "needs_lead_intervention"
                );
                assert_eq!(
                    operations,
                    vec![
                        BoundaryOperation::CapabilityProbe,
                        BoundaryOperation::NoProposalOwnerPark
                    ]
                );
            }
            Fixture::Disabled | Fixture::ExplicitLegacy => {
                assert!(
                    matches!(outcome, TaskRunOutcome::Failed { .. }),
                    "legacy fixture must pass the eligibility gate"
                );
                assert_eq!(operations, vec![BoundaryOperation::CapabilityProbe]);
                if matches!(fixture, Fixture::ExplicitLegacy) {
                    assert_eq!(
                        tasks
                            .get(&task.id)
                            .await
                            .unwrap()
                            .unwrap()
                            .pr_url
                            .as_deref(),
                        Some("https://example.test/pr/42")
                    );
                }
            }
        }
    }
}

/// This harness deliberately owns the persisted installation, provider, and
/// local-git prerequisites. It invokes production supervisor and poller entry
/// points; the test seams only redirect their real transport endpoints.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supported_disabled_retained_legacy_adopts_through_every_poller() {
    let _lifecycle_guard = RETAINED_LEGACY_LIFECYCLE_GUARD.lock().await;
    use crate::{
        direct_delivery::{BoundaryOperation, clear_boundary_operations, take_boundary_operations},
        pr_poller::installation::set_installation_client_base_url_for_test,
        supervisor_impl::{SupervisorCallbackContext, supervisor_pr_open},
    };
    use djinn_core::{
        events::EventBus,
        models::{KnowledgeInjectionConfig, TaskRunTrigger, TransitionAction},
    };
    use djinn_db::{Database, EpicRepository, TaskRepository};
    use djinn_provider::github_app::installations::prime_cache_for_tests;
    use djinn_runtime::{SupervisorFlow, TaskRunOutcome, TaskRunSpec};
    use djinn_workspace::MirrorManager;
    use std::{collections::HashMap, sync::Arc};
    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    const INSTALLATION: u64 = 42_424;
    const URL: &str = "https://github.com/acme/widget/pull/73";
    const HEAD: &str = "1111111111111111111111111111111111111111";
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
    let pr = serde_json::json!({"number":73,"title":"retained","state":"open","merged":false,"html_url":URL,"head":{"ref":"task/retained","sha":HEAD},"base":{"ref":"main","sha":"base"},"node_id":"PR_retained"});
    Mock::given(method("GET"))
        .and(path("/repos/acme/widget/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([pr.clone()])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widget/pulls/73"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/acme/widget/commits/{HEAD}/check-runs"
        )))
        .respond_with(
            // Keep the real status poller in `pr_draft` after it crosses its
            // production minimum-age guard; the explicit undraft below then
            // makes the review phase unambiguous.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"total_count":1,"check_runs":[{"id":1,"name":"ci","status":"in_progress","conclusion":null,"html_url":"https://example.test/check/1"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widget/pulls/73/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let db = Database::open_in_memory().unwrap();
    let events = EventBus::noop();
    let epic = EpicRepository::new(db.clone(), events.clone())
        .create("legacy", "", "", "", "", None)
        .await
        .unwrap();
    let tasks = TaskRepository::new(db.clone(), events);
    let task = tasks
        .create(
            &epic.id,
            "retained",
            "",
            "",
            "task",
            0,
            "worker",
            Some("approved"),
        )
        .await
        .unwrap();
    assert!(task.pr_url.is_none());
    djinn_db::test_support::persist_project_github_installation_for_test(
        &db,
        &task.project_id,
        "acme",
        "widget",
        INSTALLATION,
    )
    .await;
    prime_cache_for_tests(INSTALLATION, "ghs_installation_fixture");

    let root = crate::test_helpers::test_tempdir("retained-legacy-git-");
    let mirror_root = root.path().join("mirrors");
    std::fs::create_dir_all(&mirror_root).unwrap();
    let bare = mirror_root.join(format!("{}.git", task.project_id));
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
    set_installation_client_base_url_for_test(Some(server.uri()));
    super::set_push_url_override_for_test(Some(format!("file://{}", bare.display())));
    let mut context =
        crate::test_helpers::coordinator_context_from_db(db.clone(), CancellationToken::new());
    context.mirror = Some(Arc::new(MirrorManager::new(mirror_root)));
    let spec = TaskRunSpec {
        task_run_id: "run".into(),
        task_attempt_id: None,
        task_id: task.id.clone(),
        execution_generation: 0,
        project_id: task.project_id.clone(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "task/retained".into(),
        flow: SupervisorFlow::NewTask,
        model_id_per_role: HashMap::new(),
        read_source_project_ids: vec![],
        knowledge_injection: KnowledgeInjectionConfig::default(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
    };
    let callbacks = SupervisorCallbackContext {
        agent_context: context,
        cancel: CancellationToken::new(),
        provider_override: None,
    };
    clear_boundary_operations();
    assert!(
        matches!(supervisor_pr_open(&spec, &tasks.get(&task.id).await.unwrap().unwrap(), &callbacks).await, TaskRunOutcome::PrOpened { ref url, .. } if url == URL)
    );
    assert_eq!(
        tasks
            .get(&task.id)
            .await
            .unwrap()
            .unwrap()
            .pr_url
            .as_deref(),
        Some(URL)
    );

    let (tx, _) = tokio::sync::broadcast::channel(8);
    let (mut actor, cancel) = crate::test_helpers::make_coordinator_actor_cancellable(&db, &tx);
    // The first pass records the draft's first-seen instant. The second real
    // status-poller pass crosses its production age guard and reaches GitHub.
    actor.poll_pr_statuses().await;
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    actor.poll_pr_statuses().await;
    assert_eq!(
        tasks
            .get(&task.id)
            .await
            .unwrap()
            .unwrap()
            .pr_url
            .as_deref(),
        Some(URL)
    );
    tasks
        .transition(
            &task.id,
            TransitionAction::PrUndraft,
            "fixture",
            "system",
            None,
            None,
        )
        .await
        .unwrap();
    actor.poll_pr_review_tasks().await;
    assert_eq!(
        tasks
            .get(&task.id)
            .await
            .unwrap()
            .unwrap()
            .pr_url
            .as_deref(),
        Some(URL)
    );
    tasks
        .transition(
            &task.id,
            TransitionAction::PrCiFailed,
            "fixture",
            "system",
            None,
            None,
        )
        .await
        .unwrap();
    actor.reconcile_blindspot_merged_prs().await;
    assert_eq!(
        tasks
            .get(&task.id)
            .await
            .unwrap()
            .unwrap()
            .pr_url
            .as_deref(),
        Some(URL)
    );
    let operations = take_boundary_operations();
    for expected in [
        BoundaryOperation::SupervisorPrOpen,
        BoundaryOperation::TaskPrLookup,
        BoundaryOperation::TaskPrAdopt,
        BoundaryOperation::TaskPrStatusPoll,
        BoundaryOperation::TaskPrReviewPoll,
        BoundaryOperation::TaskPrMergedPoll,
    ] {
        assert!(
            operations.contains(&expected),
            "missing {expected:?}: {operations:?}"
        );
    }
    assert!(!operations.contains(&BoundaryOperation::DirectAppend));
    super::set_push_url_override_for_test(None);
    set_installation_client_base_url_for_test(None);
    cancel.cancel();
}
/// SupportedActive retains this one task on the legacy task-PR route only because
/// its explicit legacy marker was durably written before its repository epoch was
/// activated. The lifecycle remains the real supervisor and production pollers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supported_active_explicit_legacy_adopts_through_every_poller() {
    let _lifecycle_guard = RETAINED_LEGACY_LIFECYCLE_GUARD.lock().await;
    use crate::{
        direct_delivery::{
            BoundaryOperation, LEGACY_DELIVERY_LABEL, clear_boundary_operations,
            take_boundary_operations,
        },
        pr_poller::installation::set_installation_client_base_url_for_test,
        supervisor_impl::{SupervisorCallbackContext, supervisor_pr_open},
    };
    use djinn_core::{
        events::EventBus,
        models::{KnowledgeInjectionConfig, TaskRunTrigger, TransitionAction},
    };
    use djinn_db::{Database, EpicRepository, TaskRepository};
    use djinn_provider::github_app::installations::prime_cache_for_tests;
    use djinn_runtime::{SupervisorFlow, TaskRunOutcome, TaskRunSpec};
    use djinn_workspace::MirrorManager;
    use std::{collections::HashMap, sync::Arc};
    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    const INSTALLATION: u64 = 42_424;
    const URL: &str = "https://github.com/acme/widget/pull/73";
    const HEAD: &str = "1111111111111111111111111111111111111111";
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
    let pr = serde_json::json!({"number":73,"title":"retained","state":"open","merged":false,"html_url":URL,"head":{"ref":"task/retained","sha":HEAD},"base":{"ref":"main","sha":"base"},"node_id":"PR_retained"});
    Mock::given(method("GET"))
        .and(path("/repos/acme/widget/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([pr.clone()])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widget/pulls/73"))
        .respond_with(ResponseTemplate::new(200).set_body_json(pr))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/acme/widget/commits/{HEAD}/check-runs"
        )))
        .respond_with(
            // Keep the real status poller in `pr_draft` after it crosses its
            // production minimum-age guard; the explicit undraft below then
            // makes the review phase unambiguous.
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"total_count":1,"check_runs":[{"id":1,"name":"ci","status":"in_progress","conclusion":null,"html_url":"https://example.test/check/1"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widget/pulls/73/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&server)
        .await;

    let db = Database::open_in_memory().unwrap();
    let events = EventBus::noop();
    let epic = EpicRepository::new(db.clone(), events.clone())
        .create("legacy", "", "", "", "", None)
        .await
        .unwrap();
    let tasks = TaskRepository::new(db.clone(), events);
    let task = tasks
        .create(
            &epic.id,
            "retained",
            "",
            "",
            "task",
            0,
            "worker",
            Some("approved"),
        )
        .await
        .unwrap();
    assert!(task.pr_url.is_none());
    let explicit_legacy_labels = format!(r#"["{LEGACY_DELIVERY_LABEL}"]"#);
    tasks
        .update_labels(&task.id, &explicit_legacy_labels)
        .await
        .unwrap();
    assert_eq!(
        tasks.get(&task.id).await.unwrap().unwrap().labels,
        explicit_legacy_labels,
        "the explicit-legacy marker must be persisted through TaskRepository"
    );
    // Activate only this in-memory fixture's persisted epoch after the task has
    // its explicit legacy identity; no proposal ownership is seeded or resolved.
    djinn_db::test_support::activate_direct_delivery_epoch_for_test(&db).await;
    djinn_db::test_support::persist_project_github_installation_for_test(
        &db,
        &task.project_id,
        "acme",
        "widget",
        INSTALLATION,
    )
    .await;
    prime_cache_for_tests(INSTALLATION, "ghs_installation_fixture");

    let root = crate::test_helpers::test_tempdir("retained-legacy-git-");
    let mirror_root = root.path().join("mirrors");
    std::fs::create_dir_all(&mirror_root).unwrap();
    let bare = mirror_root.join(format!("{}.git", task.project_id));
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
    set_installation_client_base_url_for_test(Some(server.uri()));
    super::set_push_url_override_for_test(Some(format!("file://{}", bare.display())));
    let mut context =
        crate::test_helpers::coordinator_context_from_db(db.clone(), CancellationToken::new());
    context.mirror = Some(Arc::new(MirrorManager::new(mirror_root)));
    let spec = TaskRunSpec {
        task_run_id: "run".into(),
        task_attempt_id: None,
        task_id: task.id.clone(),
        execution_generation: 0,
        project_id: task.project_id.clone(),
        trigger: TaskRunTrigger::NewTask,
        base_branch: "main".into(),
        task_branch: "task/retained".into(),
        flow: SupervisorFlow::NewTask,
        model_id_per_role: HashMap::new(),
        read_source_project_ids: vec![],
        knowledge_injection: KnowledgeInjectionConfig::default(),
        github_owner: None,
        github_install_token: None,
        commit_author_name: None,
        commit_author_email: None,
        resume_lifecycle_metadata: None,
        is_evidence_spike: false,
    };
    let callbacks = SupervisorCallbackContext {
        agent_context: context,
        cancel: CancellationToken::new(),
        provider_override: None,
    };
    clear_boundary_operations();
    assert!(
        matches!(supervisor_pr_open(&spec, &tasks.get(&task.id).await.unwrap().unwrap(), &callbacks).await, TaskRunOutcome::PrOpened { ref url, .. } if url == URL)
    );
    assert_eq!(
        tasks
            .get(&task.id)
            .await
            .unwrap()
            .unwrap()
            .pr_url
            .as_deref(),
        Some(URL)
    );

    let (tx, _) = tokio::sync::broadcast::channel(8);
    let (mut actor, cancel) = crate::test_helpers::make_coordinator_actor_cancellable(&db, &tx);
    // The first pass records the draft's first-seen instant. The second real
    // status-poller pass crosses its production age guard and reaches GitHub.
    actor.poll_pr_statuses().await;
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    actor.poll_pr_statuses().await;
    assert_eq!(
        tasks
            .get(&task.id)
            .await
            .unwrap()
            .unwrap()
            .pr_url
            .as_deref(),
        Some(URL)
    );
    tasks
        .transition(
            &task.id,
            TransitionAction::PrUndraft,
            "fixture",
            "system",
            None,
            None,
        )
        .await
        .unwrap();
    actor.poll_pr_review_tasks().await;
    assert_eq!(
        tasks
            .get(&task.id)
            .await
            .unwrap()
            .unwrap()
            .pr_url
            .as_deref(),
        Some(URL)
    );
    tasks
        .transition(
            &task.id,
            TransitionAction::PrCiFailed,
            "fixture",
            "system",
            None,
            None,
        )
        .await
        .unwrap();
    actor.reconcile_blindspot_merged_prs().await;
    assert_eq!(
        tasks
            .get(&task.id)
            .await
            .unwrap()
            .unwrap()
            .pr_url
            .as_deref(),
        Some(URL)
    );
    let operations = take_boundary_operations();
    for expected in [
        BoundaryOperation::SupervisorPrOpen,
        BoundaryOperation::TaskPrLookup,
        BoundaryOperation::TaskPrAdopt,
        BoundaryOperation::TaskPrStatusPoll,
        BoundaryOperation::TaskPrReviewPoll,
        BoundaryOperation::TaskPrMergedPoll,
    ] {
        assert!(
            operations.contains(&expected),
            "missing {expected:?}: {operations:?}"
        );
    }
    for forbidden in [
        BoundaryOperation::DirectAppend,
        BoundaryOperation::TaskPrCreate,
    ] {
        assert!(
            !operations.contains(&forbidden),
            "explicit legacy must not reach forbidden operation {forbidden:?}: {operations:?}"
        );
    }
    super::set_push_url_override_for_test(None);
    set_installation_client_base_url_for_test(None);
    cancel.cancel();
}
