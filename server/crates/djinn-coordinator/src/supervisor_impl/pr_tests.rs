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

/// Compare every persisted task field while explicitly accounting for the two
/// mutations that a retained legacy supervisor adoption is allowed to make.
/// In particular, do not discard parking evidence: an unexpected escalation
/// epoch is an ownership mutation even if the task still has a task PR URL.
fn assert_legacy_supervisor_task_snapshot(
    before: &djinn_core::models::Task,
    after: &djinn_core::models::Task,
    expected_pr_url: &str,
) {
    assert_eq!(
        before.pr_url, None,
        "the supervisor snapshot must begin without a task-PR identity"
    );
    assert_eq!(
        after.pr_url.as_deref(),
        Some(expected_pr_url),
        "the only newly-owned PR identity must be the adopted task PR"
    );
    assert_eq!(after.status, "pr_draft");
    assert_eq!(after.escalation_evidence_at, before.escalation_evidence_at);
    let mut expected = serde_json::to_value(before).unwrap();
    let expected = expected.as_object_mut().unwrap();
    expected.insert("status".into(), serde_json::json!("pr_draft"));
    expected.insert("pr_url".into(), serde_json::json!(expected_pr_url));
    // Task-PR adoption deliberately changes only its lifecycle, URL, and
    // persistence timestamp; every other durable task field remains exact.
    expected.insert("updated_at".into(), serde_json::json!(after.updated_at));
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::Value::Object(expected.clone())
    );
}

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
        BoundaryOperation, LEGACY_DELIVERY_LABEL, boundary_operations_scope,
    };
    use crate::supervisor_impl::{SupervisorCallbackContext, supervisor_pr_open};
    use djinn_core::events::EventBus;
    use djinn_core::models::{KnowledgeInjectionConfig, TaskRunTrigger};
    use djinn_db::{
        ActivateProposalBuildAttemptInput, Database, DirectDeliveryCapabilityRepository,
        EpicRepository, PersistAttemptPrIdentityInput, ProposalBuildAttemptRepository,
        ReserveProposalBuildAttemptInput, TaskRepository,
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

    /// Parking may change only these two task fields. Every call site must
    /// account for each removed field explicitly rather than treating a partial
    /// JSON comparison as a full-task snapshot.
    fn task_snapshot_except_expected_park(task: &djinn_core::models::Task) -> serde_json::Value {
        let mut task = serde_json::to_value(task).unwrap();
        {
            let task = task.as_object_mut().unwrap();
            for field in ["status", "updated_at"] {
                task.remove(field);
            }
        }
        task
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
        djinn_db::test_support::activate_direct_delivery_epoch_for_test(&db).await;
        if matches!(fixture, Fixture::Direct) {
            djinn_db::test_support::seed_direct_delivery_proposal_owner_for_test(
                &db, &epic.id, "p", "p",
            )
            .await;
        } else {
            djinn_db::test_support::seed_direct_delivery_proposal_for_test(&db, "p", "p").await;
        }
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
        attempts
            .persist_pr_identity(&PersistAttemptPrIdentityInput {
                build_attempt_id: "a".into(),
                proposal_pr_number: 314,
                proposal_pr_url: "https://example.test/attempt-pr/314".into(),
            })
            .await
            .unwrap();
        // Snapshot exact ownership while the epoch contract is readable. The
        // missing/unknown fixtures intentionally make epoch-gated repositories
        // unavailable after their mutation below.
        let task = tasks.get(&task.id).await.unwrap().unwrap();
        let before_task = task.clone();
        let before_attempt = attempts.get("a").await.unwrap().unwrap();
        let before_ledger = tasks
            .latest_delivery_for_attempt("a", &task.id)
            .await
            .unwrap();
        let before_counts =
            djinn_db::test_support::direct_delivery_matrix_counts_for_test(&db).await;
        assert_eq!(
            before_counts.build_attempts,
            Some(1),
            "fixture must contain exactly the seeded attempt PR owner"
        );
        assert_eq!(
            before_counts.attempt_pr_identities,
            Some(1),
            "fixture must contain exactly one complete attempt PR identity"
        );
        assert_eq!(
            before_counts.deliveries,
            Some(0),
            "fixture must begin with an exact empty delivery ledger"
        );
        match fixture {
            Fixture::Disabled => {
                djinn_db::test_support::disable_direct_delivery_epoch_for_test(&db).await
            }
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
        let before_epoch = DirectDeliveryCapabilityRepository::new(db.clone())
            .probe()
            .await
            .map_err(|error| error.to_string());
        let boundary_operations = boundary_operations_scope().await;
        let boundary_checkpoint = boundary_operations.checkpoint();
        let outcome = supervisor_pr_open(&spec, &task, &callbacks).await;
        let operations = boundary_operations.operations_since(boundary_checkpoint);
        for forbidden in [
            BoundaryOperation::TaskPrLookup,
            BoundaryOperation::TaskPrAdopt,
            BoundaryOperation::TaskPrCreate,
            BoundaryOperation::TaskPrInlineCleanup,
            BoundaryOperation::TaskPrStaleCleanup,
            BoundaryOperation::TaskPrMerge,
            BoundaryOperation::TaskPrAutoMerge,
            BoundaryOperation::TaskPrApproval,
            BoundaryOperation::TaskPrSignoff,
            BoundaryOperation::TaskPrCustomEnqueue,
            BoundaryOperation::AttemptPrCreateOrAdoptRequest,
        ] {
            assert!(
                !operations.contains(&forbidden),
                "direct-delivery boundary reached forbidden task-PR effect {forbidden:?}"
            );
        }
        let after_task = tasks.get(&task.id).await.unwrap().unwrap();
        let after_epoch = DirectDeliveryCapabilityRepository::new(db.clone())
            .probe()
            .await
            .map_err(|error| error.to_string());
        assert_eq!(
            after_epoch, before_epoch,
            "supervisor must preserve the exact readable or fail-closed epoch probe"
        );
        if matches!(fixture, Fixture::MissingContract | Fixture::UnknownContract) {
            // Restore only the test epoch state so the gated repositories can
            // reload and compare the pre-existing ownership rows exactly.
            djinn_db::test_support::restore_active_direct_delivery_epoch_for_test(&db).await;
        }
        let after_attempt = attempts.get("a").await.unwrap().unwrap();
        let after_ledger = tasks
            .latest_delivery_for_attempt("a", &task.id)
            .await
            .unwrap();
        let after_counts =
            djinn_db::test_support::direct_delivery_matrix_counts_for_test(&db).await;
        assert_eq!(
            after_attempt, before_attempt,
            "attempt row and exact PR identity must remain unchanged"
        );
        assert_eq!(
            after_ledger, before_ledger,
            "exact ledger identities/cardinality must remain unchanged"
        );
        assert_eq!(
            after_counts, before_counts,
            "supervisor must not create attempts or ledger rows"
        );
        assert_eq!(after_attempt.proposal_pr_number, Some(314));
        assert_eq!(
            after_attempt.proposal_pr_url.as_deref(),
            Some("https://example.test/attempt-pr/314")
        );
        match fixture {
            Fixture::Direct => {
                assert!(matches!(outcome, TaskRunOutcome::Escalated { .. }));
                assert_eq!(
                    serde_json::to_value(&after_task).unwrap(),
                    serde_json::to_value(&before_task).unwrap(),
                    "direct supervisor exclusion must preserve the full task row"
                );
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
                assert_eq!(after_task.status, "needs_lead_intervention");
                assert_eq!(after_task.pr_url, before_task.pr_url);
                assert_ne!(
                    after_task.updated_at, before_task.updated_at,
                    "parking must persist its timestamp"
                );
                assert_eq!(
                    after_task.escalation_evidence_at, before_task.escalation_evidence_at,
                    "parking must preserve escalation evidence"
                );
                assert_eq!(
                    task_snapshot_except_expected_park(&after_task),
                    task_snapshot_except_expected_park(&before_task)
                );
                assert!(
                    tasks
                        .list_activity(&task.id)
                        .await
                        .unwrap()
                        .last()
                        .unwrap()
                        .payload
                        .contains("no_proposal_owner")
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
                assert_eq!(after_task.status, "needs_lead_intervention");
                assert_eq!(after_task.pr_url, before_task.pr_url);
                assert_ne!(
                    after_task.updated_at, before_task.updated_at,
                    "parking must persist its timestamp"
                );
                assert_eq!(
                    after_task.escalation_evidence_at, before_task.escalation_evidence_at,
                    "parking must preserve escalation evidence"
                );
                assert_eq!(
                    task_snapshot_except_expected_park(&after_task),
                    task_snapshot_except_expected_park(&before_task)
                );
                let reason = if matches!(fixture, Fixture::MissingContract) {
                    "direct_delivery_contract_missing_epoch"
                } else {
                    "direct_delivery_contract_unknown_epoch"
                };
                assert!(
                    tasks
                        .list_activity(&task.id)
                        .await
                        .unwrap()
                        .last()
                        .unwrap()
                        .payload
                        .contains(reason)
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
                assert_eq!(
                    serde_json::to_value(&after_task).unwrap(),
                    serde_json::to_value(&before_task).unwrap(),
                    "legacy eligibility must preserve the full task row"
                );
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
        direct_delivery::{BoundaryOperation, boundary_operations_scope},
        pr_poller::installation::set_installation_client_base_url_for_test,
        supervisor_impl::{SupervisorCallbackContext, supervisor_pr_open},
    };
    use djinn_core::{
        events::EventBus,
        models::{KnowledgeInjectionConfig, TaskRunTrigger, TransitionAction},
    };
    use djinn_db::{
        ActivateProposalBuildAttemptInput, Database, DirectDeliveryCapabilityRepository,
        EpicRepository, PersistAttemptPrIdentityInput, ProposalBuildAttemptRepository,
        ReserveProposalBuildAttemptInput, TaskRepository,
    };
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
    // An unrelated persisted attempt PR must remain exact and cardinality-stable
    // while this disabled-epoch task takes its retained legacy route.
    djinn_db::test_support::seed_direct_delivery_proposal_for_test(&db, "p", "p").await;
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
    djinn_db::test_support::activate_direct_delivery_epoch_for_test(&db).await;
    attempts
        .activate(&ActivateProposalBuildAttemptInput {
            build_attempt_id: "a".into(),
            expected_lifecycle: djinn_core::models::ProposalBuildAttemptLifecycle::Reserved,
            expected_branch_head_sha: None,
            branch_head_sha: "base".into(),
        })
        .await
        .unwrap();
    attempts
        .persist_pr_identity(&PersistAttemptPrIdentityInput {
            build_attempt_id: "a".into(),
            proposal_pr_number: 314,
            proposal_pr_url: "https://example.test/attempt-pr/314".into(),
        })
        .await
        .unwrap();
    djinn_db::test_support::disable_direct_delivery_epoch_for_test(&db).await;

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
    let before_task = tasks.get(&task.id).await.unwrap().unwrap();
    let before_attempt = attempts.get("a").await.unwrap().unwrap();
    let before_ledger = tasks
        .latest_delivery_for_attempt("a", &task.id)
        .await
        .unwrap();
    let before_counts = djinn_db::test_support::direct_delivery_matrix_counts_for_test(&db).await;
    assert_eq!(
        before_counts.build_attempts,
        Some(1),
        "SupportedDisabled fixture must contain exactly one attempt PR identity"
    );
    assert_eq!(
        before_counts.attempt_pr_identities,
        Some(1),
        "SupportedDisabled fixture must contain exactly one complete attempt PR identity"
    );
    assert_eq!(
        before_counts.deliveries,
        Some(0),
        "SupportedDisabled fixture must snapshot an exact empty ledger"
    );
    let before_epoch = DirectDeliveryCapabilityRepository::new(db.clone())
        .probe()
        .await
        .unwrap();
    let boundary_operations = boundary_operations_scope().await;
    let boundary_checkpoint = boundary_operations.checkpoint();
    assert!(
        matches!(supervisor_pr_open(&spec, &tasks.get(&task.id).await.unwrap().unwrap(), &callbacks).await, TaskRunOutcome::PrOpened { ref url, .. } if url == URL)
    );
    let after_task = tasks.get(&task.id).await.unwrap().unwrap();
    assert_legacy_supervisor_task_snapshot(&before_task, &after_task, URL);
    assert_eq!(after_task.pr_url.as_deref(), Some(URL));
    let after_attempt = attempts.get("a").await.unwrap().unwrap();
    assert_eq!(after_attempt, before_attempt);
    assert_eq!(after_attempt.proposal_pr_number, Some(314));
    assert_eq!(
        after_attempt.proposal_pr_url.as_deref(),
        Some("https://example.test/attempt-pr/314")
    );
    assert_eq!(
        tasks
            .latest_delivery_for_attempt("a", &task.id)
            .await
            .unwrap(),
        before_ledger
    );
    assert_eq!(
        djinn_db::test_support::direct_delivery_matrix_counts_for_test(&db).await,
        before_counts
    );
    assert_eq!(
        DirectDeliveryCapabilityRepository::new(db.clone())
            .probe()
            .await
            .unwrap(),
        before_epoch
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
    let operations = boundary_operations.operations_since(boundary_checkpoint);
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
        BoundaryOperation::AttemptPrCreateOrAdoptRequest,
    ] {
        assert!(
            !operations.contains(&forbidden),
            "disabled retained legacy must not reach attempt ownership operation {forbidden:?}: {operations:?}"
        );
    }
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
        direct_delivery::{BoundaryOperation, LEGACY_DELIVERY_LABEL, boundary_operations_scope},
        pr_poller::installation::set_installation_client_base_url_for_test,
        supervisor_impl::{SupervisorCallbackContext, supervisor_pr_open},
    };
    use djinn_core::{
        events::EventBus,
        models::{KnowledgeInjectionConfig, TaskRunTrigger, TransitionAction},
    };
    use djinn_db::{
        ActivateProposalBuildAttemptInput, Database, DirectDeliveryCapabilityRepository,
        EpicRepository, PersistAttemptPrIdentityInput, ProposalBuildAttemptRepository,
        ReserveProposalBuildAttemptInput, TaskRepository,
    };
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
    // This active-epoch explicit-legacy task must not take ownership of an
    // existing attempt PR belonging to an unrelated proposal.
    djinn_db::test_support::seed_direct_delivery_proposal_for_test(&db, "p", "p").await;
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
    attempts
        .persist_pr_identity(&PersistAttemptPrIdentityInput {
            build_attempt_id: "a".into(),
            proposal_pr_number: 315,
            proposal_pr_url: "https://example.test/attempt-pr/315".into(),
        })
        .await
        .unwrap();

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
    let before_task = tasks.get(&task.id).await.unwrap().unwrap();
    let before_attempt = attempts.get("a").await.unwrap().unwrap();
    let before_ledger = tasks
        .latest_delivery_for_attempt("a", &task.id)
        .await
        .unwrap();
    let before_counts = djinn_db::test_support::direct_delivery_matrix_counts_for_test(&db).await;
    assert_eq!(
        before_counts.build_attempts,
        Some(1),
        "explicit-legacy fixture must contain exactly one attempt PR identity"
    );
    assert_eq!(
        before_counts.attempt_pr_identities,
        Some(1),
        "explicit-legacy fixture must contain exactly one complete attempt PR identity"
    );
    assert_eq!(
        before_counts.deliveries,
        Some(0),
        "explicit-legacy fixture must snapshot an exact empty ledger"
    );
    let before_epoch = DirectDeliveryCapabilityRepository::new(db.clone())
        .probe()
        .await
        .unwrap();
    let boundary_operations = boundary_operations_scope().await;
    let boundary_checkpoint = boundary_operations.checkpoint();
    assert!(
        matches!(supervisor_pr_open(&spec, &tasks.get(&task.id).await.unwrap().unwrap(), &callbacks).await, TaskRunOutcome::PrOpened { ref url, .. } if url == URL)
    );
    let after_task = tasks.get(&task.id).await.unwrap().unwrap();
    assert_legacy_supervisor_task_snapshot(&before_task, &after_task, URL);
    assert_eq!(after_task.pr_url.as_deref(), Some(URL));
    let after_attempt = attempts.get("a").await.unwrap().unwrap();
    assert_eq!(after_attempt, before_attempt);
    assert_eq!(after_attempt.proposal_pr_number, Some(315));
    assert_eq!(
        after_attempt.proposal_pr_url.as_deref(),
        Some("https://example.test/attempt-pr/315")
    );
    assert_eq!(
        tasks
            .latest_delivery_for_attempt("a", &task.id)
            .await
            .unwrap(),
        before_ledger
    );
    assert_eq!(
        djinn_db::test_support::direct_delivery_matrix_counts_for_test(&db).await,
        before_counts
    );
    assert_eq!(
        DirectDeliveryCapabilityRepository::new(db.clone())
            .probe()
            .await
            .unwrap(),
        before_epoch
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
    let operations = boundary_operations.operations_since(boundary_checkpoint);
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
        BoundaryOperation::AttemptPrCreateOrAdoptRequest,
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

/// Run both production cleanup routes for disabled and active explicit-legacy
/// tasks. Counted provider requests rule out all relevant early returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_legacy_cleanup_reaches_inline_and_stale_provider_boundaries() {
    let _lifecycle_guard = RETAINED_LEGACY_LIFECYCLE_GUARD.lock().await;
    use crate::{
        direct_delivery::{BoundaryOperation, LEGACY_DELIVERY_LABEL, boundary_operations_scope},
        health,
        pr_poller::{
            installation::set_installation_client_base_url_for_test, pr_cleanup::CloseKind,
        },
    };
    use djinn_core::events::EventBus;
    use djinn_db::{Database, EpicRepository, TaskRepository};
    use djinn_provider::github_app::installations::prime_cache_for_tests;
    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };

    const INSTALLATION: u64 = 42_425;
    const URL: &str = "https://github.com/acme/widget/pull/74";
    const TITLE: &str = "retainclean";
    const HEAD: &str = "2222222222222222222222222222222222222222";

    for (active, stale) in [(false, false), (false, true), (true, false), (true, true)] {
        let db = Database::open_in_memory().unwrap();
        let events = EventBus::noop();
        let epic = EpicRepository::new(db.clone(), events.clone())
            .create("legacy cleanup", "", "", "", "", None)
            .await
            .unwrap();
        let tasks = TaskRepository::new(db.clone(), events);
        let task = tasks
            .create(
                &epic.id,
                TITLE,
                "",
                "",
                "task",
                0,
                "worker",
                Some("approved"),
            )
            .await
            .unwrap();
        let task_branch = format!("task/{}", task.short_id);

        let server = MockServer::start().await;
        let pr = serde_json::json!({"number":74,"title":"retained cleanup","state":"open","merged":false,"html_url":URL,"user":{"login":"djinn-bot[bot]","id":1},"head":{"ref":task_branch,"sha":HEAD},"base":{"ref":"main","sha":"base"},"node_id":"PR_retained_cleanup"});
        let graphql = serde_json::json!({"data":{"repository":{"pullRequest":{"mergeStateStatus":null,"autoMergeRequest":null,"mergeQueueEntry":null,"timelineItems":{"nodes":[]},"commits":{"nodes":[]}}}}});
        if stale {
            Mock::given(method("GET"))
                .and(path("/repos/acme/widget/pulls"))
                .and(query_param("state", "open"))
                .and(query_param("per_page", "100"))
                .and(query_param("page", "1"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!([pr.clone()])),
                )
                .expect(1)
                .mount(&server)
                .await;
        } else {
            Mock::given(method("GET"))
                .and(path("/repos/acme/widget/pulls/74"))
                .respond_with(ResponseTemplate::new(200).set_body_json(pr.clone()))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(format!(
                    "/repos/acme/widget/commits/{HEAD}/check-runs"
                )))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"total_count":0,"check_runs":[]})),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(graphql))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/widget/pulls"))
            .and(query_param("base", task_branch.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/acme/widget/issues/74/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/acme/widget/pulls/74"))
            .respond_with(ResponseTemplate::new(200).set_body_json(pr.clone()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path(format!(
                "/repos/acme/widget/git/refs/heads/{task_branch}"
            )))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        if active {
            tasks
                .update_labels(&task.id, &format!(r#"["{LEGACY_DELIVERY_LABEL}"]"#))
                .await
                .unwrap();
            djinn_db::test_support::activate_direct_delivery_epoch_for_test(&db).await;
        }
        let adopted = tasks.set_pr_url(&task.id, URL).await.unwrap();
        assert_eq!(
            adopted.pr_url.as_deref(),
            Some(URL),
            "fixture must begin with adopted legacy URL"
        );
        // Both routes use the persisted close timestamp for their guardrails.
        // Backdate it through the shared repository fixture seam so the inline
        // policy's fixed grace period and the stale sweep are positively eligible.
        djinn_db::test_support::close_task_at(&db, &task.id, "2020-01-01T00:00:00Z").await;
        assert_eq!(
            tasks
                .get(&task.id)
                .await
                .unwrap()
                .unwrap()
                .pr_url
                .as_deref(),
            Some(URL),
            "backdating cleanup eligibility must not alter the adopted URL"
        );
        djinn_db::test_support::persist_project_github_installation_for_test(
            &db,
            &task.project_id,
            "acme",
            "widget",
            INSTALLATION,
        )
        .await;
        prime_cache_for_tests(INSTALLATION, "ghs_installation_fixture");
        unsafe { std::env::set_var("GITHUB_APP_ID", "1") };
        set_installation_client_base_url_for_test(Some(server.uri()));
        let boundary_operations = boundary_operations_scope().await;
        let boundary_checkpoint = boundary_operations.checkpoint();

        if stale {
            let mut context = crate::test_helpers::coordinator_context_from_db(
                db.clone(),
                CancellationToken::new(),
            );
            context.reconciliation_sweep.enabled = true;
            context.reconciliation_sweep.dry_run = false;
            context.reconciliation_sweep.grace_period = std::time::Duration::ZERO;
            health::sweep_stale_resources(&db, &context).await;
        } else {
            let (actor, cancel) = {
                let (tx, _) = tokio::sync::broadcast::channel(8);
                crate::test_helpers::make_coordinator_actor_cancellable(&db, &tx)
            };
            actor
                .cleanup_pr_and_branch_on_close(
                    &tasks.get(&task.id).await.unwrap().unwrap(),
                    CloseKind::NonMerge,
                )
                .await;
            cancel.cancel();
        }

        let reloaded = tasks.get(&task.id).await.unwrap().unwrap();
        assert_eq!(
            reloaded.pr_url.as_deref(),
            Some(URL),
            "cleanup must retain the exact adopted URL"
        );
        let operations = boundary_operations.operations_since(boundary_checkpoint);
        let expected = if stale {
            BoundaryOperation::TaskPrStaleCleanup
        } else {
            BoundaryOperation::TaskPrInlineCleanup
        };
        assert!(
            operations.contains(&expected),
            "missing {expected:?}: {operations:?}"
        );
        assert!(
            !operations.contains(&BoundaryOperation::DirectAppend),
            "legacy cleanup must not append directly: {operations:?}"
        );
        set_installation_client_base_url_for_test(None);
    }
}

/// Fresh repository fixtures prevent one maintenance route's parking from
/// de-selecting the fixture for another route.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_pr_maintenance_surfaces_independently_preserve_attempt_ownership() {
    let _guard = RETAINED_LEGACY_LIFECYCLE_GUARD.lock().await;
    use crate::{
        direct_delivery::{BoundaryOperation, boundary_operations_scope},
        health,
        pr_poller::{
            installation::set_installation_client_base_url_for_test, pr_cleanup::CloseKind,
        },
    };
    use djinn_core::events::EventBus;
    use djinn_db::{
        ActivateProposalBuildAttemptInput, Database, DirectDeliveryCapabilityRepository,
        EpicRepository, PersistAttemptPrIdentityInput, ProposalBuildAttemptRepository,
        ReserveProposalBuildAttemptInput, TaskRepository,
    };
    use djinn_provider::github_app::installations::prime_cache_for_tests;
    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, query_param},
    };
    const INSTALLATION: u64 = 42_426;
    const URL: &str = "https://github.com/acme/widget/pull/76";
    const ATTEMPT_URL: &str = "https://example.test/attempt-pr/316";
    #[derive(Clone, Copy, Debug)]
    enum Surface {
        Status,
        Review,
        Blindspot,
        Inline,
        Stale,
    }
    impl Surface {
        fn name(self) -> &'static str {
            match self {
                Self::Status => "poll_pr_statuses",
                Self::Review => "poll_pr_review_tasks",
                Self::Blindspot => "reconcile_blindspot_merged_prs",
                Self::Inline => "cleanup_pr_and_branch_on_close",
                Self::Stale => "health::sweep_stale_resources",
            }
        }
        fn status(self) -> &'static str {
            match self {
                Self::Status => "pr_draft",
                Self::Review => "pr_review",
                Self::Blindspot | Self::Inline | Self::Stale => "approved",
            }
        }
    }
    #[derive(Clone, Copy, Debug)]
    enum Fixture {
        Direct,
        Unresolved,
        Missing,
        Unknown,
    }
    impl Fixture {
        fn name(self) -> &'static str {
            match self {
                Self::Direct => "active-direct",
                Self::Unresolved => "unresolved-owner",
                Self::Missing => "missing-contract",
                Self::Unknown => "unknown-contract",
            }
        }
        fn reason(self) -> Option<&'static str> {
            match self {
                Self::Direct => None,
                Self::Unresolved => Some("no_proposal_owner"),
                Self::Missing => Some("direct_delivery_contract_missing_epoch"),
                Self::Unknown => Some("direct_delivery_contract_unknown_epoch"),
            }
        }
    }
    fn without_park(task: &djinn_core::models::Task) -> serde_json::Value {
        let mut v = serde_json::to_value(task).unwrap();
        let o = v.as_object_mut().unwrap();
        o.remove("status");
        o.remove("updated_at");
        v
    }
    for surface in [
        Surface::Status,
        Surface::Review,
        Surface::Blindspot,
        Surface::Inline,
        Surface::Stale,
    ] {
        for fixture in [
            Fixture::Direct,
            Fixture::Unresolved,
            Fixture::Missing,
            Fixture::Unknown,
        ] {
            let db = Database::open_in_memory().unwrap();
            let events = EventBus::noop();
            let epic = EpicRepository::new(db.clone(), events.clone())
                .create("independent ownership", "", "", "", "", None)
                .await
                .unwrap();
            let tasks = TaskRepository::new(db.clone(), events);
            let task = tasks
                .create(
                    &epic.id,
                    &format!("{}-{}", surface.name(), fixture.name()),
                    "",
                    "",
                    "task",
                    0,
                    "worker",
                    Some(surface.status()),
                )
                .await
                .unwrap();
            let task = tasks.set_pr_url(&task.id, URL).await.unwrap();
            djinn_db::test_support::activate_direct_delivery_epoch_for_test(&db).await;
            if matches!(fixture, Fixture::Direct) {
                djinn_db::test_support::seed_direct_delivery_proposal_owner_for_test(
                    &db, &epic.id, "p", "p",
                )
                .await;
            } else {
                djinn_db::test_support::seed_direct_delivery_proposal_for_test(&db, "p", "p").await;
            }
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
            attempts
                .persist_pr_identity(&PersistAttemptPrIdentityInput {
                    build_attempt_id: "a".into(),
                    proposal_pr_number: 316,
                    proposal_pr_url: ATTEMPT_URL.into(),
                })
                .await
                .unwrap();
            let before_task = tasks.get(&task.id).await.unwrap().unwrap();
            let before_attempt = attempts.get("a").await.unwrap().unwrap();
            let before_ledger = tasks
                .latest_delivery_for_attempt("a", &task.id)
                .await
                .unwrap();
            let before_counts =
                djinn_db::test_support::direct_delivery_matrix_counts_for_test(&db).await;
            assert_eq!(
                (
                    before_counts.build_attempts,
                    before_counts.attempt_pr_identities,
                    before_counts.deliveries
                ),
                (Some(1), Some(1), Some(0))
            );
            match fixture {
                Fixture::Missing => {
                    djinn_db::test_support::remove_direct_delivery_epoch_for_test(&db).await
                }
                Fixture::Unknown => {
                    djinn_db::test_support::seed_unknown_direct_delivery_epoch_for_test(&db).await
                }
                Fixture::Direct | Fixture::Unresolved => {}
            }
            // Observe the selected fixture contract immediately before its
            // single production entry-point call.
            let before_epoch = DirectDeliveryCapabilityRepository::new(db.clone())
                .probe()
                .await
                .map_err(|e| e.to_string());
            let server = MockServer::start().await;
            // This is the functioning provider's attempt-PR failure seam: any accidental request fails the fixture.
            Mock::given(method("POST"))
                .and(path("/repos/acme/widget/pulls"))
                .respond_with(ResponseTemplate::new(500).set_body_string("ProviderFailure"))
                .expect(0)
                .mount(&server)
                .await;
            if matches!(surface, Surface::Stale) {
                let branch = format!("task/{}", task.short_id);
                let open = serde_json::json!({"number":76,"title":"open","state":"open","merged":false,"html_url":URL,"user":{"login":"djinn-bot[bot]","id":1},"head":{"ref":branch,"sha":"head"},"base":{"ref":"main","sha":"base"},"node_id":"PR_task"});
                Mock::given(method("GET"))
                    .and(path("/repos/acme/widget/pulls"))
                    .and(query_param("state", "open"))
                    .and(query_param("per_page", "100"))
                    .and(query_param("page", "1"))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_json(serde_json::json!([open])),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
            }
            djinn_db::test_support::persist_project_github_installation_for_test(
                &db,
                &task.project_id,
                "acme",
                "widget",
                INSTALLATION,
            )
            .await;
            prime_cache_for_tests(INSTALLATION, "ghs_independent_ownership_fixture");
            unsafe { std::env::set_var("GITHUB_APP_ID", "1") };
            set_installation_client_base_url_for_test(Some(server.uri()));
            let boundary_operations = boundary_operations_scope().await;
            let boundary_checkpoint = boundary_operations.checkpoint();
            match surface {
                Surface::Status => {
                    let (tx, _) = tokio::sync::broadcast::channel(8);
                    let (mut a, c) =
                        crate::test_helpers::make_coordinator_actor_cancellable(&db, &tx);
                    a.poll_pr_statuses().await;
                    c.cancel();
                }
                Surface::Review => {
                    let (tx, _) = tokio::sync::broadcast::channel(8);
                    let (mut a, c) =
                        crate::test_helpers::make_coordinator_actor_cancellable(&db, &tx);
                    a.poll_pr_review_tasks().await;
                    c.cancel();
                }
                Surface::Blindspot => {
                    let (tx, _) = tokio::sync::broadcast::channel(8);
                    let (a, c) = crate::test_helpers::make_coordinator_actor_cancellable(&db, &tx);
                    a.reconcile_blindspot_merged_prs().await;
                    c.cancel();
                }
                Surface::Inline => {
                    let (tx, _) = tokio::sync::broadcast::channel(8);
                    let (a, c) = crate::test_helpers::make_coordinator_actor_cancellable(&db, &tx);
                    a.cleanup_pr_and_branch_on_close(
                        &tasks.get(&task.id).await.unwrap().unwrap(),
                        CloseKind::NonMerge,
                    )
                    .await;
                    c.cancel();
                }
                Surface::Stale => {
                    let mut ctx = crate::test_helpers::coordinator_context_from_db(
                        db.clone(),
                        CancellationToken::new(),
                    );
                    ctx.reconciliation_sweep.enabled = true;
                    ctx.reconciliation_sweep.dry_run = false;
                    ctx.reconciliation_sweep.grace_period = std::time::Duration::ZERO;
                    health::sweep_stale_resources(&db, &ctx).await;
                }
            }
            let operations = boundary_operations.operations_since(boundary_checkpoint);
            let after_task = tasks.get(&task.id).await.unwrap().unwrap();
            assert_eq!(
                DirectDeliveryCapabilityRepository::new(db.clone())
                    .probe()
                    .await
                    .map_err(|e| e.to_string()),
                before_epoch,
                "{} {} changed epoch",
                surface.name(),
                fixture.name()
            );
            if matches!(fixture, Fixture::Missing | Fixture::Unknown) {
                djinn_db::test_support::restore_active_direct_delivery_epoch_for_test(&db).await;
            }
            let after_attempt = attempts.get("a").await.unwrap().unwrap();
            let after_ledger = tasks
                .latest_delivery_for_attempt("a", &task.id)
                .await
                .unwrap();
            let after_counts =
                djinn_db::test_support::direct_delivery_matrix_counts_for_test(&db).await;
            assert_eq!(
                after_attempt,
                before_attempt,
                "{} {} changed attempt",
                surface.name(),
                fixture.name()
            );
            assert_eq!(after_ledger, before_ledger);
            assert_eq!(after_counts, before_counts);
            assert_eq!(
                (
                    after_attempt.proposal_pr_number,
                    after_attempt.proposal_pr_url.as_deref()
                ),
                (Some(316), Some(ATTEMPT_URL))
            );
            assert_eq!(after_task.pr_url, before_task.pr_url);
            for forbidden in [
                BoundaryOperation::TaskPrLookup,
                BoundaryOperation::TaskPrAdopt,
                BoundaryOperation::TaskPrStatusPoll,
                BoundaryOperation::TaskPrReviewPoll,
                BoundaryOperation::TaskPrMergedPoll,
                BoundaryOperation::TaskPrInlineCleanup,
                BoundaryOperation::TaskPrCreate,
                BoundaryOperation::TaskPrMerge,
                BoundaryOperation::TaskPrAutoMerge,
                BoundaryOperation::TaskPrApproval,
                BoundaryOperation::TaskPrSignoff,
                BoundaryOperation::TaskPrCustomEnqueue,
                BoundaryOperation::AttemptPrCreateOrAdoptRequest,
                BoundaryOperation::DirectAppend,
            ] {
                assert!(
                    !operations.contains(&forbidden),
                    "{} {} reached {forbidden:?}: {operations:?}",
                    surface.name(),
                    fixture.name()
                );
            }
            if matches!(surface, Surface::Stale) {
                assert!(
                    operations.contains(&BoundaryOperation::TaskPrStaleCleanup),
                    "the stale fixture must reach per-task eligibility after its open-PR response"
                );
            } else {
                assert!(
                    !operations.contains(&BoundaryOperation::TaskPrStaleCleanup),
                    "{} unexpectedly reached stale task-PR cleanup",
                    surface.name()
                );
            }
            if let Some(reason) = fixture.reason() {
                assert_eq!(after_task.status, "needs_lead_intervention");
                assert_eq!(without_park(&after_task), without_park(&before_task));
                assert!(
                    tasks
                        .list_activity(&task.id)
                        .await
                        .unwrap()
                        .last()
                        .unwrap()
                        .payload
                        .contains(reason)
                );
            } else {
                assert_eq!(
                    serde_json::to_value(&after_task).unwrap(),
                    serde_json::to_value(&before_task).unwrap()
                );
            }
            set_installation_client_base_url_for_test(None);
        }
    }
}

/// END-TO-END REGRESSION for tasks `4vnt` (PR #3153) and `3kza` (PR #3155).
///
/// Drives the **real** `poll_pr_draft_tasks` — production entry point, real
/// installation-token client, real CI snapshot recording, real tripwire
/// active-hold reconciliation — against a PR that GitHub reports as merged
/// while an active `tripwire.gate.held` finding sits on that PR's live head.
/// Only the HTTP endpoint is redirected, via the existing
/// `set_installation_client_base_url_for_test` seam.
///
/// Before the fix the loop evaluated `reconcile_tripwire_hold` first, saw an
/// active hold, and `continue`d before ever reading `pr.merged`. Both
/// production tasks therefore sat in `pr_draft` with `closed_at: null` — 5 days
/// for `4vnt` — and their dependents (`296y`, `i58q`) stayed blocked until an
/// operator closed them by hand. A merged PR is ground truth: no Djinn-side
/// advance gate may precede observing it.
///
/// The fixture reproduces the force-push shape too: the stored CI-snapshot head
/// is the pre-force-push SHA, so a merge check keyed off any stored SHA would
/// still strand the task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merged_pr_under_active_tripwire_hold_still_closes_its_pr_draft_task() {
    let _lifecycle_guard = RETAINED_LEGACY_LIFECYCLE_GUARD.lock().await;
    use crate::pr_poller::installation::set_installation_client_base_url_for_test;
    use crate::tripwires::{
        TRIPWIRE_EVENT_GATE_HELD,
        activity_payloads::{
            TripwireEvidenceSpan, TripwireFindingSummary, TripwireGateDecisionPayload,
            TripwireSeverity,
        },
    };
    use djinn_core::events::EventBus;
    use djinn_core::models::{CiStatus, TaskPrCiSnapshotInput};
    use djinn_db::{Database, EpicRepository, TaskRepository};
    use djinn_provider::github_app::installations::prime_cache_for_tests;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    const INSTALLATION: u64 = 42_425;
    const URL: &str = "https://github.com/acme/widget/pull/3155";
    /// The head GitHub reports and merged.
    const MERGED_HEAD: &str = "fef4a526f115c7e5ca6b86cf76856676389f176f";
    /// The pre-force-push head Djinn still had stored (the `3kza` divergence).
    const STALE_STORED_HEAD: &str = "0d67ee6e892124e38221a515b466b3b7a582f4d1";
    const MERGE_COMMIT: &str = "06253f5ab0b48f14769598a8cded90a013d90206";

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widget/pulls/3155"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "number": 3155,
            "title": "merged while held",
            // GitHub reports merged PRs as closed; `merged` is the ground truth.
            "state": "closed",
            "merged": true,
            "merge_commit_sha": MERGE_COMMIT,
            "html_url": URL,
            "head": {"ref": "task/3kza", "sha": MERGED_HEAD},
            "base": {"ref": "main", "sha": "base"},
            "node_id": "PR_3kza"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/acme/widget/commits/{MERGED_HEAD}/check-runs"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 1,
            "check_runs": [{
                "id": 1, "name": "ci", "status": "completed", "conclusion": "success",
                "html_url": "https://example.test/check/1"
            }]
        })))
        .mount(&server)
        .await;

    let db = Database::open_in_memory().unwrap();
    let events = EventBus::noop();
    let epic = EpicRepository::new(db.clone(), events.clone())
        .create("merged-under-hold", "", "", "", "", None)
        .await
        .unwrap();
    let tasks = TaskRepository::new(db.clone(), events);
    let task = tasks
        .create(
            &epic.id,
            "merged while tripwire-held",
            "",
            "",
            "task",
            0,
            "worker",
            Some("pr_draft"),
        )
        .await
        .unwrap();
    tasks.set_pr_url(&task.id, URL).await.unwrap();
    djinn_db::test_support::persist_project_github_installation_for_test(
        &db,
        &task.project_id,
        "acme",
        "widget",
        INSTALLATION,
    )
    .await;
    prime_cache_for_tests(INSTALLATION, "ghs_installation_fixture");

    // The dependency chain that silently dropped out of the build loop.
    let dependent = tasks
        .create(&epic.id, "dependent", "", "", "task", 0, "worker", None)
        .await
        .unwrap();
    tasks.add_blocker(&dependent.id, &task.id).await.unwrap();

    // Stale stored head — the force-push shape both incidents carried.
    tasks
        .upsert_ci_snapshot(TaskPrCiSnapshotInput {
            task_id: task.id.clone(),
            pr_number: 3155,
            head_sha: STALE_STORED_HEAD.into(),
            ci_status: CiStatus::Passing,
            blocking_required_check_names: vec![],
            primary_blocking_check: None,
            failure_annotations: None,
            failure_fingerprint: None,
            same_signature_count: 0,
            last_remediation_base_sha: None,
        })
        .await
        .unwrap();

    // The active tripwire hold on the PR's LIVE head — the gate that used to
    // mask merge detection for five days.
    let finding = TripwireFindingSummary {
        rule_id: "large_delete_or_rewrite".into(),
        reason_code: "tripwire.large_delete_or_rewrite".into(),
        severity: TripwireSeverity::HumanReviewRequired,
        evidence: TripwireEvidenceSpan {
            path: "server/crates/djinn-coordinator/src/rules.rs".into(),
            start_line: None,
            end_line: None,
            evidence_redacted: false,
        },
        idempotency_key: "sha256:held-finding-1".into(),
        content_fingerprint: "fp:sha256:held-finding-1".into(),
        downgrade_reason: None,
    };
    let held = TripwireGateDecisionPayload {
        event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
        task_id: task.id.clone(),
        project_id: task.project_id.clone(),
        pr_number: Some(3155),
        head_sha: MERGED_HEAD.into(),
        base_sha: None,
        policy_revision: "org-policy:default".into(),
        allowlist_revision: None,
        findings: vec![finding],
        enforcement_finding_count: 1,
        report_only_finding_count: 0,
        idempotency_key: "sha256:held-gate-1".into(),
        decided_at: None,
    };
    tasks
        .log_activity(
            Some(&task.id),
            "coordinator",
            "system",
            TRIPWIRE_EVENT_GATE_HELD,
            &serde_json::to_string(&held).unwrap(),
        )
        .await
        .unwrap();

    unsafe { std::env::set_var("GITHUB_APP_ID", "1") };
    set_installation_client_base_url_for_test(Some(server.uri()));

    let (tx, _rx) = tokio::sync::broadcast::channel(8);
    let (mut actor, cancel) = crate::test_helpers::make_coordinator_actor_cancellable(&db, &tx);
    // Satisfy the production minimum-age guard without a 10s sleep: pretend the
    // task has been in `pr_draft` for a minute. Every other gate is real.
    actor.pr_draft_first_seen.insert(
        task.id.clone(),
        std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(60))
            .expect("monotonic clock is at least 60s past boot"),
    );

    // Sanity: the hold really is active on the live head, so this test is
    // exercising the masked path and not a trivially-unheld one.
    let reloaded = tasks.get(&task.id).await.unwrap().unwrap();
    assert!(
        actor
            .reconcile_tripwire_hold(&reloaded, 3155, MERGED_HEAD)
            .await,
        "precondition: an active tripwire hold must be present on the merged head"
    );

    actor.poll_pr_draft_tasks().await;

    let closed = tasks.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        closed.status, "closed",
        "a merged PR must close its task even while a tripwire hold is active on \
         its head — the hold gates ADVANCING a PR, not observing one that already \
         merged (incidents 4vnt/#3153 and 3kza/#3155)"
    );
    assert_eq!(closed.close_reason.as_deref(), Some("completed"));
    assert!(closed.closed_at.is_some(), "closed_at must be populated");
    assert_eq!(
        closed.merge_commit_sha.as_deref(),
        Some(MERGE_COMMIT),
        "a task closed as merged must record which commit merged it"
    );

    let ready: Vec<String> = tasks
        .list_ready(djinn_db::ReadyQuery::default())
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert!(
        ready.contains(&dependent.id),
        "closing the merged task must release its blocked dependent"
    );

    set_installation_client_base_url_for_test(None);
    cancel.cancel();
    db.pool().close().await;
}
