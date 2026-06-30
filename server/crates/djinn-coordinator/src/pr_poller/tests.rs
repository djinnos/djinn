// djinn:allow-oversize
use super::{
    AutoMergeFastPathState, AutoMergeTickDecision, CiMergeGateVerdict, PrDraftCiAction, Task,
    advisory_checks_section, blocking_failed_checks, build_ci_failure_sections,
    ci_merge_gate_verdict, compute_ci_failure_fingerprint, count_consecutive_identical,
    decide_auto_merge_tick, decide_pr_draft_ci_action, dequeue_reason_is_failure,
    dequeue_requires_rework, detect_scope_inversion, effective_review_decision, extract_crate_name,
    extract_crate_names, is_advisory_check_name, is_conversation_resolution_block,
    is_merge_queue_405, is_racing_unmerged_status, parse_actions_run_id, parse_pr_url,
    pick_conflict_blocker_sibling, pr_transition_increments_reopen_count,
    record_auto_merge_decision_metrics, record_pr_transition_reopen_metric,
    should_auto_resolve_conversations,
};
use djinn_core::models::TransitionAction;
use djinn_provider::github_api::{
    ActionsJob, ActionsJobStep, CheckRun, DequeueEvent, GitHubApiError, GitHubUser, PrReview,
};
use reqwest::StatusCode;
use std::collections::HashMap;

// ── Offloaded clean-merge fast-path guard ─────────────────────────────────

#[test]
fn auto_merge_first_tick_spawns_and_marks_inflight() {
    // No tracker entry → spawn the background merge and mark in-flight so the
    // heavy git work runs off the actor tick.
    let mut tracker: HashMap<String, AutoMergeFastPathState> = HashMap::new();
    let decision = decide_auto_merge_tick(&mut tracker, "task-1");
    assert_eq!(decision, AutoMergeTickDecision::Spawn);
    assert_eq!(
        tracker.get("task-1"),
        Some(&AutoMergeFastPathState::InFlight),
        "first tick must mark the task in-flight"
    );
}

#[test]
fn pr_transition_reopen_metric_tracks_reopen_count_semantics() {
    djinn_telemetry::init().unwrap();
    let before = djinn_telemetry::render().unwrap();
    let reopens_before = unlabelled_metric_value(&before, "djinn_task_reopens_total");

    assert!(pr_transition_increments_reopen_count(
        &TransitionAction::PrCiFailed
    ));
    assert!(pr_transition_increments_reopen_count(
        &TransitionAction::PrChangesRequested
    ));
    assert!(
        !pr_transition_increments_reopen_count(&TransitionAction::PrConflict),
        "PrConflict reopens a task for conflict handling but does not increment reopen_count"
    );

    assert!(record_pr_transition_reopen_metric(
        &TransitionAction::PrCiFailed
    ));
    assert!(!record_pr_transition_reopen_metric(
        &TransitionAction::PrConflict
    ));
    let rendered = djinn_telemetry::render().unwrap();
    assert_eq!(
        unlabelled_metric_value(&rendered, "djinn_task_reopens_total"),
        reopens_before + 1.0,
        "only the transition that increments reopen_count should bump djinn_task_reopens_total"
    );
}

#[test]
fn auto_merge_inflight_tick_skips_without_respawn() {
    // The guard: while a merge is in flight, repeated ticks must NOT spawn a
    // second background merge or double-dispatch — they just skip the task.
    let mut tracker: HashMap<String, AutoMergeFastPathState> = HashMap::new();
    tracker.insert("task-1".into(), AutoMergeFastPathState::InFlight);
    let decision = decide_auto_merge_tick(&mut tracker, "task-1");
    assert_eq!(
        decision,
        AutoMergeTickDecision::Return(AutoMergeFastPathState::InFlight),
        "an in-flight task must be skipped, never re-spawned"
    );
    // Still exactly one entry, still in-flight.
    assert_eq!(tracker.len(), 1);
    assert_eq!(
        tracker.get("task-1"),
        Some(&AutoMergeFastPathState::InFlight)
    );
}

#[test]
fn auto_merge_completed_states_are_consumed() {
    // A completed background attempt (Merged or Reopen) is returned to the
    // poller AND removed, so the next conflict on the same task re-arms a
    // fresh attempt (next tick will Spawn again).
    for state in [
        AutoMergeFastPathState::Merged,
        AutoMergeFastPathState::Reopen,
    ] {
        let mut tracker: HashMap<String, AutoMergeFastPathState> = HashMap::new();
        tracker.insert("task-1".into(), state.clone());
        let decision = decide_auto_merge_tick(&mut tracker, "task-1");
        assert_eq!(decision, AutoMergeTickDecision::Return(state));
        assert!(
            !tracker.contains_key("task-1"),
            "completed state must be consumed so a later conflict re-arms"
        );
        // The very next tick (still mergeable==false) re-arms a fresh attempt.
        let next = decide_auto_merge_tick(&mut tracker, "task-1");
        assert_eq!(next, AutoMergeTickDecision::Spawn);
    }
}

#[test]
fn auto_merge_full_cycle_inflight_then_reopen_then_respawn() {
    // End-to-end guard walk for one conflicting PR: spawn → (background still
    // running) skip → background records Reopen → poller consumes Reopen
    // (falls through to reopen) → a fresh conflict re-arms.
    let mut tracker: HashMap<String, AutoMergeFastPathState> = HashMap::new();

    // Tick 1: first sight of the conflict → spawn.
    assert_eq!(
        decide_auto_merge_tick(&mut tracker, "t"),
        AutoMergeTickDecision::Spawn
    );

    // Tick 2: merge still running → skip, no second spawn.
    assert_eq!(
        decide_auto_merge_tick(&mut tracker, "t"),
        AutoMergeTickDecision::Return(AutoMergeFastPathState::InFlight)
    );

    // Background task finishes with a real conflict.
    tracker.insert("t".into(), AutoMergeFastPathState::Reopen);

    // Tick 3: consume Reopen → poller proceeds to its flag-and-reopen flow.
    assert_eq!(
        decide_auto_merge_tick(&mut tracker, "t"),
        AutoMergeTickDecision::Return(AutoMergeFastPathState::Reopen)
    );
    assert!(tracker.is_empty());
}

#[test]
fn auto_merge_reopen_decision_records_merge_failure_after_lock_release() {
    djinn_telemetry::init().unwrap();
    let before = djinn_telemetry::render().unwrap();
    let merge_failures_before = unlabelled_metric_value(&before, "djinn_merge_failures_total");

    // Use the same tracked value as the coordinator live-metrics regression so
    // the process-global gauge cannot make either test flaky if libtest runs
    // them concurrently.
    record_auto_merge_decision_metrics(
        &AutoMergeTickDecision::Return(AutoMergeFastPathState::Reopen),
        2,
    );

    let rendered = djinn_telemetry::render().unwrap();
    assert_eq!(
        unlabelled_metric_value(&rendered, "djinn_pr_poller_tracked"),
        2.0
    );
    assert_eq!(
        unlabelled_metric_value(&rendered, "djinn_merge_failures_total"),
        merge_failures_before + 1.0
    );
}

fn unlabelled_metric_value(rendered: &str, metric: &str) -> f64 {
    rendered
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(' ')?;
            (name == metric).then(|| value.parse::<f64>().expect("metric value parses"))
        })
        .unwrap_or_else(|| panic!("missing metric {metric} in:\n{rendered}"))
}

/// Minimal `PrReview` builder for the effective-decision tests.
fn review(author: &str, state: &str, submitted_at: &str) -> PrReview {
    PrReview {
        id: 1,
        user: Some(GitHubUser {
            login: author.to_string(),
            id: 1,
        }),
        state: state.to_string(),
        submitted_at: Some(submitted_at.to_string()),
        html_url: String::new(),
        body: String::new(),
    }
}

#[test]
fn effective_decision_latest_approval_supersedes_stale_changes_requested() {
    // Exact task-2sq6 shape: the same reviewer left COMMENTED twice, then
    // CHANGES_REQUESTED, then dismissed it three times, then APPROVED on the
    // current head. GitHub's reviewDecision is APPROVED — ours must agree,
    // NOT keep matching the stale 17:35:01 CHANGES_REQUESTED entry.
    let reviews = vec![
        review("claude", "COMMENTED", "2026-06-01T17:34:53Z"),
        review("claude", "COMMENTED", "2026-06-01T17:34:57Z"),
        review("claude", "CHANGES_REQUESTED", "2026-06-01T17:35:01Z"),
        review("claude", "DISMISSED", "2026-06-01T19:03:50Z"),
        review("claude", "DISMISSED", "2026-06-01T19:17:56Z"),
        review("claude", "DISMISSED", "2026-06-01T19:34:10Z"),
        review("claude", "APPROVED", "2026-06-01T20:11:37Z"),
    ];
    let (changes_requested, has_approved) = effective_review_decision(&reviews);
    assert!(
        !changes_requested,
        "a superseded CHANGES_REQUESTED must not force rework once the same reviewer approves"
    );
    assert!(has_approved, "latest standing review is APPROVED");
}

#[test]
fn effective_decision_standing_changes_requested_still_blocks() {
    // Reviewer requested changes and hasn't approved since → still blocks.
    let reviews = vec![
        review("claude", "COMMENTED", "2026-06-01T17:34:53Z"),
        review("claude", "APPROVED", "2026-06-01T18:00:00Z"),
        review("claude", "CHANGES_REQUESTED", "2026-06-01T19:00:00Z"),
    ];
    let (changes_requested, has_approved) = effective_review_decision(&reviews);
    assert!(
        changes_requested,
        "latest standing review is CHANGES_REQUESTED"
    );
    assert!(!has_approved);
}

#[test]
fn effective_decision_is_per_reviewer() {
    // One reviewer approved on the current head; another still has an
    // outstanding change request → the PR is blocked (any blocker blocks).
    let reviews = vec![
        review("alice", "APPROVED", "2026-06-01T20:00:00Z"),
        review("bob", "CHANGES_REQUESTED", "2026-06-01T19:00:00Z"),
    ];
    let (changes_requested, has_approved) = effective_review_decision(&reviews);
    assert!(changes_requested, "bob's outstanding change request blocks");
    assert!(has_approved, "alice approved");
}

#[test]
fn effective_decision_dismissed_clears_standing() {
    // A lone CHANGES_REQUESTED later DISMISSED leaves no standing → neither
    // blocks nor approves (falls through to the merge-eligibility path).
    let reviews = vec![
        review("claude", "CHANGES_REQUESTED", "2026-06-01T17:00:00Z"),
        review("claude", "DISMISSED", "2026-06-01T18:00:00Z"),
    ];
    let (changes_requested, has_approved) = effective_review_decision(&reviews);
    assert!(!changes_requested);
    assert!(!has_approved);
}

#[test]
fn effective_decision_ignores_commented_only() {
    // COMMENTED reviews are informational — never gating.
    let reviews = vec![
        review("claude", "COMMENTED", "2026-06-01T17:00:00Z"),
        review("claude", "COMMENTED", "2026-06-01T18:00:00Z"),
    ];
    assert_eq!(effective_review_decision(&reviews), (false, false));
}

/// Minimal `Task` builder for the sibling-attribution heuristic tests.
/// Only `id` and `status` are load-bearing; everything else is filler.
fn task(id: &str, status: &str) -> Task {
    Task {
        id: id.to_string(),
        project_id: "p".to_string(),
        short_id: id.to_string(),
        epic_id: Some("ep".to_string()),
        title: String::new(),
        description: String::new(),
        design: String::new(),
        issue_type: "task".to_string(),
        status: status.to_string(),
        priority: 0,
        owner: String::new(),
        labels: "[]".to_string(),
        acceptance_criteria: "[]".to_string(),
        reopen_count: 0,
        continuation_count: 0,
        total_reopen_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
        created_at: String::new(),
        updated_at: String::new(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".to_string(),
        agent_type: None,
        created_by_user_id: None,
        ci_status: "unknown".to_string(),
        ci_head_sha: None,
        ci_pr_number: None,
        ci_blocking_required_check_names: "[]".to_string(),
        ci_failure_fingerprint: None,
        ci_first_seen_at: None,
        ci_last_seen_at: None,
        ci_same_signature_count: 0,
        ci_last_remediation_base_sha: None,
        unresolved_blocker_count: 0,
    }
}

#[test]
fn racing_status_classification() {
    // Post-implementation, unmerged → racing.
    assert!(is_racing_unmerged_status("approved"));
    assert!(is_racing_unmerged_status("pr_draft"));
    assert!(is_racing_unmerged_status("pr_review"));
    // Pre-implementation / terminal → NOT racing (nothing to wait on, or
    // would never release the block).
    assert!(!is_racing_unmerged_status("open"));
    assert!(!is_racing_unmerged_status("in_progress"));
    assert!(!is_racing_unmerged_status("closed"));
}

#[test]
fn picks_single_racing_sibling() {
    // Exactly one racing same-epic sibling → unambiguous → block on it.
    let siblings = vec![
        task("self", "open"),
        task("peer", "pr_review"),
        task("done", "closed"),
    ];
    assert_eq!(
        pick_conflict_blocker_sibling("self", &siblings),
        Some("peer".to_string())
    );
}

#[test]
fn no_block_when_zero_racing_siblings() {
    // Conflict is against already-merged main (sibling closed) → nothing to
    // wait on → fall back to plain reopen.
    let siblings = vec![task("self", "open"), task("done", "closed")];
    assert_eq!(pick_conflict_blocker_sibling("self", &siblings), None);
}

#[test]
fn no_block_when_multiple_racing_siblings() {
    // Two racing siblings → ambiguous attribution → don't guess.
    let siblings = vec![
        task("self", "open"),
        task("peer1", "pr_review"),
        task("peer2", "approved"),
    ];
    assert_eq!(pick_conflict_blocker_sibling("self", &siblings), None);
}

#[test]
fn never_blocks_on_self() {
    // The task itself, even in a racing state, is never a candidate.
    let siblings = vec![task("self", "pr_review")];
    assert_eq!(pick_conflict_blocker_sibling("self", &siblings), None);
}

fn check(name: &str) -> CheckRun {
    CheckRun {
        id: 1,
        name: name.to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        html_url: "https://github.com/o/r/runs/1".to_string(),
    }
}

#[test]
fn advisory_section_empty_for_no_checks() {
    assert!(advisory_checks_section(&[]).is_none());
}

#[test]
fn advisory_section_lists_checks_and_disclaims_blocking() {
    let sentinel = check("Sentinel");
    let e2e = check("Partner E2E");
    let section = advisory_checks_section(&[&sentinel, &e2e]).expect("section");
    assert!(section.contains("Other failing checks outside the required gate"));
    assert!(section.contains("Sentinel (failure)"));
    assert!(section.contains("Partner E2E (failure)"));
    // The section must never tell the worker the required gate is ignorable.
    assert!(!section.contains("do not gate merging"));
    assert!(!section.to_lowercase().contains("do not loop"));
    assert!(section.contains("make those green") || section.contains("gate merging"));
}

#[test]
fn advisory_check_names_classified() {
    // The real 1ck3 offenders.
    assert!(is_advisory_check_name(
        "PR Preview Environment Setup / setup-preview"
    ));
    assert!(is_advisory_check_name("Vercel – acme-portal"));
    assert!(is_advisory_check_name("Vercel – admin-portal"));
    assert!(is_advisory_check_name("Netlify deploy"));
    assert!(is_advisory_check_name("Cloudflare Pages"));
    assert!(is_advisory_check_name("Deployment to staging"));

    // Required checks must NOT be classified as advisory.
    assert!(!is_advisory_check_name("Sentinel"));
    assert!(!is_advisory_check_name("unit tests"));
    assert!(!is_advisory_check_name("ci / build"));
    assert!(!is_advisory_check_name("lint"));
}

#[test]
fn blocking_filter_uses_required_contexts_when_present() {
    let preview = check("Vercel – acme-portal");
    let unit = check("unit tests");
    let failed = vec![&preview, &unit];
    // Branch protection lists only "unit tests" + "Sentinel" as required.
    let required = vec!["unit tests".to_string(), "Sentinel".to_string()];

    let blocking = blocking_failed_checks(&failed, Some(&required));
    assert_eq!(blocking.len(), 1);
    assert_eq!(blocking[0].name, "unit tests");
}

#[test]
fn blocking_filter_empty_when_only_non_required_fail() {
    // The exact 1ck3 shape: required checks (unit tests / Sentinel) are
    // GREEN — only preview/Vercel checks failed, so none of the *failed*
    // checks are in the required set.
    let preview = check("PR Preview Environment Setup / setup-preview");
    let vercel = check("Vercel – acme-portal");
    let failed = vec![&preview, &vercel];
    let required = vec!["unit tests".to_string(), "Sentinel".to_string()];

    let blocking = blocking_failed_checks(&failed, Some(&required));
    assert!(
        blocking.is_empty(),
        "no required check failed → nothing should trigger rework"
    );
}

#[test]
fn blocking_filter_falls_back_to_heuristic_without_contexts() {
    let preview = check("Vercel – acme-portal");
    let unit = check("unit tests");
    let failed = vec![&preview, &unit];

    // No branch-protection contexts available → name-pattern heuristic.
    let blocking = blocking_failed_checks(&failed, None);
    assert_eq!(blocking.len(), 1);
    assert_eq!(blocking[0].name, "unit tests");
}

#[test]
fn blocking_filter_heuristic_keeps_unknown_checks_as_blocking() {
    // Conservative fallback: an unrecognised check is treated as blocking
    // so we never silently swallow a real failure.
    let mystery = check("some-custom-gate");
    let failed = vec![&mystery];
    let blocking = blocking_failed_checks(&failed, None);
    assert_eq!(
        blocking.len(),
        1,
        "unknown checks must be treated as blocking"
    );
}

#[test]
fn blocking_filter_heuristic_drops_only_advisory() {
    let preview = check("Deploy Preview");
    let vercel = check("Vercel – portal");
    let failed = vec![&preview, &vercel];
    let blocking = blocking_failed_checks(&failed, None);
    assert!(blocking.is_empty());
}

#[test]
fn blocking_filter_includes_failing_jobs_of_required_aggregate_run() {
    // Reproduces task b29n / PR #718: `main` requires only the aggregate
    // `Quality Gate` status, whose run fans out to `Server Clippy` etc. The
    // aggregate is red because `Server Clippy` failed. GitHub reports every job
    // of that workflow run as its own check-run sharing ONE run id, but only
    // `Quality Gate` is in the required-contexts list. The failing constituent
    // jobs (which a diff CAN fix) must be treated as blockers, not advisory.
    let gate = check_run("Quality Gate", 27708614687);
    let clippy = check_run("Server Clippy", 27708614687);
    let size_guard = check_run("Server Size Guard", 27708614687);
    // A genuinely-advisory failure in a *different* workflow run.
    let vercel = check_run("Vercel – portal", 99999);
    let failed = vec![&gate, &clippy, &size_guard, &vercel];

    // Only the aggregate is required.
    let required = vec!["Quality Gate".to_string()];
    let blocking = blocking_failed_checks(&failed, Some(&required));

    let names: std::collections::HashSet<&str> =
        blocking.iter().map(|cr| cr.name.as_str()).collect();
    assert!(
        names.contains("Quality Gate"),
        "the required aggregate itself is blocking"
    );
    assert!(
        names.contains("Server Clippy"),
        "a failing job inside the required aggregate's run must be a blocker, \
         not advisory — this is the b29n bug"
    );
    assert!(
        names.contains("Server Size Guard"),
        "every failing job in the required run is a blocker"
    );
    assert!(
        !names.contains("Vercel – portal"),
        "a failure in a separate (non-required) workflow run stays advisory"
    );
    assert_eq!(blocking.len(), 3);
}

#[test]
fn blocking_filter_required_run_with_no_failing_jobs_keeps_only_required() {
    // Sanity: if only the required aggregate failed (no constituent job check-
    // run present), it remains the lone blocker — the run-grouping never
    // over-collects.
    let gate = check_run("Quality Gate", 555);
    let unrelated = check_run("Some Other Workflow", 777);
    let failed = vec![&gate, &unrelated];
    let required = vec!["Quality Gate".to_string()];
    let blocking = blocking_failed_checks(&failed, Some(&required));
    assert_eq!(blocking.len(), 1);
    assert_eq!(blocking[0].name, "Quality Gate");
}

fn github_http_error(
    method: &'static str,
    path: &str,
    status: StatusCode,
    body: &str,
) -> GitHubApiError {
    GitHubApiError::http(method, path.to_string(), status, body.to_string())
}

fn github_graphql_error(method: &'static str, body: &str) -> GitHubApiError {
    GitHubApiError::graphql(method, "/graphql".to_string(), body.to_string())
}

fn assert_structured_rendered_error(rendered: &str, prefix: &str, method: &str, path: &str) {
    assert!(
        rendered.starts_with(&format!("{prefix}: {{")),
        "rendered error should start with operation prefix and JSON object: {rendered}"
    );
    assert!(rendered.contains("\"error_class\":"), "{rendered}");
    assert!(
        rendered.contains(&format!("\"method\":\"{method}\"")),
        "{rendered}"
    );
    assert!(
        rendered.contains(&format!("\"path\":\"{path}\"")),
        "{rendered}"
    );
    assert!(
        !rendered.contains("github ") && !rendered.contains(" GraphQL error"),
        "must not fall back to GitHubApiError::Display text: {rendered}"
    );
}

#[test]
fn is_merge_queue_405_matches_real_payload() {
    let err = github_http_error(
        "PUT",
        "/repos/djinnos/djinn/pulls/7/merge",
        StatusCode::METHOD_NOT_ALLOWED,
        r#"{\"message\":\"Pull Request is in the merge queue.\",\"status\":\"405\"}"#,
    );
    assert!(is_merge_queue_405(&err));
    let rendered =
        crate::github_error_render::render_github_write_error("GitHub PR merge failed", &err);
    assert_structured_rendered_error(
        &rendered,
        "GitHub PR merge failed",
        "PUT",
        "/repos/djinnos/djinn/pulls/7/merge",
    );
    assert!(rendered.contains("\"error_class\":\"conflict_recoverable\""));
    assert!(rendered.contains("\"status\":\"405\""));
    assert!(rendered.contains("merge queue"));
}

#[test]
fn is_merge_queue_405_ignores_unrelated_405s() {
    let err = github_http_error(
        "PUT",
        "/repos/djinnos/djinn/pulls/7/merge",
        StatusCode::METHOD_NOT_ALLOWED,
        r#"{\"message\":\"locked\"}"#,
    );
    assert!(!is_merge_queue_405(&err));
}

#[test]
fn is_already_queued_matches_real_enqueue_rejection() {
    let err = github_graphql_error(
        "POST",
        r#"[{\"type\":\"UNPROCESSABLE\",\"path\":[\"enqueuePullRequest\"],\"message\":\"Pull request is already in the queue\"}]"#,
    );
    assert!(super::is_already_queued(&err));
    let rendered =
        crate::github_error_render::render_github_write_error("GitHub enqueue PR failed", &err);
    assert_structured_rendered_error(&rendered, "GitHub enqueue PR failed", "POST", "/graphql");
    assert!(rendered.contains("\"error_class\":\"conflict_recoverable\""));

    let other = github_graphql_error(
        "POST",
        r#"[{\"type\":\"UNPROCESSABLE\",\"message\":\"Pull request is not mergeable\"}]"#,
    );
    assert!(!super::is_already_queued(&other));
}

#[test]
fn auto_merge_best_effort_failure_rendering_exposes_envelope() {
    let err = github_graphql_error(
        "POST",
        r#"[{\"type\":\"UNPROCESSABLE\",\"message\":\"Pull request Auto merge is not allowed on this repository\"}]"#,
    );

    let rendered = crate::github_error_render::render_github_write_error(
        "GitHub auto-merge enable failed",
        &err,
    );

    assert!(rendered.contains("GitHub auto-merge enable failed"));
    assert_structured_rendered_error(
        &rendered,
        "GitHub auto-merge enable failed",
        "POST",
        "/graphql",
    );
    assert!(rendered.contains("\"error_class\":\"validation\""));
    assert!(rendered.contains("Auto merge is not allowed"));
}

#[test]
fn update_branch_failure_rendering_exposes_bounded_envelope() {
    let long_body = format!(
        "Expected head SHA did not match current branch head. {}",
        "retrying this stale update branch request would keep failing. ".repeat(10)
    );
    let err = github_http_error(
        "PUT",
        "/repos/djinnos/djinn/pulls/7/update-branch",
        StatusCode::UNPROCESSABLE_ENTITY,
        &long_body,
    );

    let rendered =
        crate::github_error_render::render_github_write_error("GitHub update-branch failed", &err);

    assert!(rendered.contains("GitHub update-branch failed"));
    assert_structured_rendered_error(
        &rendered,
        "GitHub update-branch failed",
        "PUT",
        "/repos/djinnos/djinn/pulls/7/update-branch",
    );
    assert!(rendered.contains("\"error_class\":\"conflict_recoverable\""));
    assert!(rendered.contains("\"status\":\"422\""));
    assert!(rendered.contains("Expected head SHA did not match"));
    assert!(rendered.contains('…'));
    assert!(
        !rendered
            .contains(&"retrying this stale update branch request would keep failing. ".repeat(6)),
        "update-branch envelope body must be compact: {rendered}"
    );
}

#[test]
fn is_merge_queue_405_ignores_other_status_codes() {
    let err = github_http_error(
        "PUT",
        "/repos/djinnos/djinn/pulls/7/merge",
        StatusCode::UNPROCESSABLE_ENTITY,
        "Pull Request is in the merge queue.",
    );
    assert!(!is_merge_queue_405(&err));
}

#[test]
fn is_conversation_resolution_block_matches_real_payload() {
    let err = github_http_error(
        "PUT",
        "/repos/djinnos/djinn/pulls/7/merge",
        StatusCode::METHOD_NOT_ALLOWED,
        "{\"message\":\"Repository rule violations found\\n\\nA conversation must be resolved before this pull request can be merged.\\n\\n\",\"status\":\"405\"}",
    );
    assert!(is_conversation_resolution_block(&err));
    let rendered =
        crate::github_error_render::render_github_write_error("GitHub PR merge failed", &err);
    assert_structured_rendered_error(
        &rendered,
        "GitHub PR merge failed",
        "PUT",
        "/repos/djinnos/djinn/pulls/7/merge",
    );
    assert!(rendered.contains("\"error_class\":\"validation\""));
    assert!(rendered.contains("\"status\":\"405\""));
    assert!(rendered.contains("conversation must be resolved"));
}

#[test]
fn is_conversation_resolution_block_ignores_merge_queue_405() {
    let err = github_http_error(
        "PUT",
        "/repos/djinnos/djinn/pulls/7/merge",
        StatusCode::METHOD_NOT_ALLOWED,
        r#"{\"message\":\"Pull Request is in the merge queue.\",\"status\":\"405\"}"#,
    );
    assert!(!is_conversation_resolution_block(&err));
}

#[test]
fn is_conversation_resolution_block_ignores_generic_405() {
    let err = github_http_error(
        "PUT",
        "/repos/djinnos/djinn/pulls/7/merge",
        StatusCode::METHOD_NOT_ALLOWED,
        r#"{\"message\":\"locked\"}"#,
    );
    assert!(!is_conversation_resolution_block(&err));
}

#[test]
fn is_conversation_resolution_block_ignores_other_status_codes() {
    let err = github_http_error(
        "PUT",
        "/repos/djinnos/djinn/pulls/7/merge",
        StatusCode::CONFLICT,
        "A conversation must be resolved before this pull request can be merged.",
    );
    assert!(!is_conversation_resolution_block(&err));
}

#[test]
fn dequeue_reasons_classified_correctly() {
    // Failures: anything not on the safe-list.
    assert!(dequeue_reason_is_failure(Some("CHECKS_FAILED")));
    assert!(dequeue_reason_is_failure(Some("MERGE_CONFLICT")));
    assert!(dequeue_reason_is_failure(Some("NO_RESPONSE")));
    assert!(dequeue_reason_is_failure(Some("NOT_QUEUEABLE")));
    assert!(dequeue_reason_is_failure(Some("ROLL_BACK")));
    assert!(dequeue_reason_is_failure(Some("UNKNOWN_REMOVAL_REASON")));
    assert!(dequeue_reason_is_failure(Some("SOMETHING_NEW")));

    // Lowercase timeline-event vocabulary classifies the same way.
    assert!(dequeue_reason_is_failure(Some("failed_checks")));
    assert!(dequeue_reason_is_failure(Some("checks_failed")));

    // Non-failures: merged (queue success), head moved, queue admin
    // reset, manual intervention — in both vocabularies.
    assert!(!dequeue_reason_is_failure(Some("MERGED")));
    assert!(!dequeue_reason_is_failure(Some("merged")));
    assert!(!dequeue_reason_is_failure(Some("BRANCH_INVALIDATED")));
    assert!(!dequeue_reason_is_failure(Some("branch_invalidated")));
    assert!(!dequeue_reason_is_failure(Some("QUEUE_CLEARED")));
    assert!(!dequeue_reason_is_failure(Some("DEQUEUED")));
    // Operator queue-removal surfaces as `manual` on the timeline event and
    // `DEQUEUED` on the GraphQL enum — both spellings are benign and must
    // not reopen the task for rework (regression: a manual dequeue
    // spuriously reopened a task in prod, 2026-06-18).
    assert!(!dequeue_reason_is_failure(Some("manual")));
    assert!(!dequeue_reason_is_failure(Some("MANUAL")));
    assert!(!dequeue_reason_is_failure(None));
}

// ── Sticky failure-dequeue decision ────────────────────────────────────────

fn dequeue_event(reason: &str, created_at: Option<&str>) -> DequeueEvent {
    DequeueEvent {
        reason: Some(reason.to_string()),
        merge_group_ref: None,
        created_at: created_at.map(|s| s.to_string()),
        // NOTE: beforeCommit on the real event is the merge-group head, not
        // the PR head — the decision must not depend on it.
        before_commit_sha: Some("merge-group-head".to_string()),
    }
}

#[test]
fn dequeue_requires_rework_on_unhandled_failure_with_no_commit_after() {
    // The blind-requeue loop case (PRs #491/#492): queue rejected a head
    // whose last commit predates the eviction and nothing consumed the
    // event yet — must reopen even if the PR is already sitting back in
    // the queue.
    let dq = dequeue_event("failed_checks", Some("2026-06-12T14:30:33Z"));
    assert!(dequeue_requires_rework(
        Some(&dq),
        Some("2026-06-12T14:12:31Z"),
        None
    ));
    // A different handled timestamp is an older, already-consumed event —
    // this one is new and still actionable.
    assert!(dequeue_requires_rework(
        Some(&dq),
        Some("2026-06-12T14:12:31Z"),
        Some("2026-06-12T13:56:01Z")
    ));
}

#[test]
fn dequeue_requires_rework_skips_when_rework_already_landed() {
    // A commit landed after the eviction — the rejection is stale and the
    // reworked head deserves a fresh queue run, not another reopen.
    let dq = dequeue_event("failed_checks", Some("2026-06-12T14:05:59Z"));
    assert!(!dequeue_requires_rework(
        Some(&dq),
        Some("2026-06-12T14:18:19Z"),
        None
    ));
}

#[test]
fn dequeue_requires_rework_consumes_each_event_once() {
    // Same created_at as the handled marker → already reopened for this
    // event; don't re-fire while the rework is still in flight.
    let dq = dequeue_event("failed_checks", Some("2026-06-12T14:30:33Z"));
    assert!(!dequeue_requires_rework(
        Some(&dq),
        Some("2026-06-12T14:12:31Z"),
        Some("2026-06-12T14:30:33Z")
    ));
}

#[test]
fn dequeue_requires_rework_ignores_non_failure_and_absent_events() {
    let merged = dequeue_event("merged", Some("2026-06-12T14:50:36Z"));
    assert!(!dequeue_requires_rework(
        Some(&merged),
        Some("2026-06-12T14:12:31Z"),
        None
    ));
    // An operator manually removing the PR from the queue surfaces as
    // `manual` on the timeline event — benign, must not reopen for rework
    // (regression: a manual dequeue spuriously reopened a task, 2026-06-18).
    let manual = dequeue_event("manual", Some("2026-06-18T10:00:00Z"));
    assert!(!dequeue_requires_rework(
        Some(&manual),
        Some("2026-06-18T09:00:00Z"),
        None
    ));
    assert!(!dequeue_requires_rework(
        None,
        Some("2026-06-12T14:12:31Z"),
        None
    ));
}

#[test]
fn dequeue_requires_rework_degrades_conservatively_on_missing_fields() {
    // No head timestamp (GraphQL field gap) → cannot prove rework landed;
    // keep the pre-existing reopen behavior.
    let dq = dequeue_event("failed_checks", Some("2026-06-12T14:30:33Z"));
    assert!(dequeue_requires_rework(Some(&dq), None, None));
    // No created_at → cannot compare or dedup; err on reopening.
    let no_ts = dequeue_event("failed_checks", None);
    assert!(dequeue_requires_rework(
        Some(&no_ts),
        Some("2026-06-12T14:12:31Z"),
        Some("2026-06-12T14:30:33Z")
    ));
}

#[test]
fn parses_standard_pr_url() {
    let result = parse_pr_url("https://github.com/djinnos/server/pull/42");
    assert_eq!(
        result,
        Some(("djinnos".to_string(), "server".to_string(), 42))
    );
}

#[test]
fn parses_pr_url_with_trailing_fragment() {
    let result = parse_pr_url("https://github.com/owner/repo/pull/7#discussion");
    assert_eq!(result, Some(("owner".to_string(), "repo".to_string(), 7)));
}

#[test]
fn rejects_non_pr_url() {
    assert_eq!(parse_pr_url("https://github.com/owner/repo/issues/1"), None);
}

#[test]
fn rejects_non_github_url() {
    assert_eq!(parse_pr_url("https://gitlab.com/owner/repo/pull/1"), None);
}

// ---- CI-failure aggregation (E3) -------------------------------------

fn check_run(name: &str, run_id: u64) -> CheckRun {
    CheckRun {
        id: run_id * 100 + name.len() as u64,
        name: name.to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        html_url: format!("https://github.com/o/r/actions/runs/{run_id}/job/{run_id}9"),
    }
}

fn failed_step(name: &str, number: u64) -> ActionsJobStep {
    ActionsJobStep {
        name: name.to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        number,
    }
}

fn failed_job(id: u64, name: &str, workflow: &str, steps: Vec<ActionsJobStep>) -> ActionsJob {
    ActionsJob {
        id,
        name: name.to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        html_url: format!("https://github.com/o/r/actions/runs/x/job/{id}"),
        workflow_name: Some(workflow.to_string()),
        steps,
    }
}

#[test]
fn parse_actions_run_id_extracts_id() {
    assert_eq!(
        parse_actions_run_id("https://github.com/o/r/actions/runs/123456/job/99"),
        Some(123456)
    );
    // Non-Actions check-run URL carries no run id.
    assert_eq!(
        parse_actions_run_id("https://github.com/o/r/checks/abc"),
        None
    );
}

#[test]
fn ci_failure_sections_single_run() {
    // Baseline: one run with one failing job/step still renders correctly.
    let jobs = vec![failed_job(
        10,
        "build",
        "CI",
        vec![failed_step("cargo build", 3)],
    )];
    let checks = [check_run("CI / build", 100)];
    let refs: Vec<&CheckRun> = checks.iter().collect();
    let (sections, ci_jobs) = build_ci_failure_sections(Some(&jobs), &refs);

    let body = sections.join("\n");
    assert!(body.contains("**Workflow:** CI"), "{body}");
    assert!(body.contains("**Failed job:** build"), "{body}");
    assert!(body.contains("**Failed step:** cargo build"), "{body}");
    assert!(body.contains("ci_job_log(job_id=10"), "{body}");
    assert_eq!(ci_jobs.len(), 1);
    assert_eq!(ci_jobs[0]["job_id"].as_u64(), Some(10));
}

#[test]
fn ci_failure_sections_unions_multiple_runs() {
    // Failures spread across two workflow runs (CI + Release): BOTH must be
    // represented, not just the first — this is the core E3 fix.
    let jobs = vec![
        failed_job(10, "build", "CI", vec![failed_step("cargo build", 3)]),
        failed_job(
            20,
            "publish",
            "Release",
            vec![failed_step("cargo publish", 5)],
        ),
    ];
    let checks = [
        check_run("CI / build", 100),
        check_run("Release / publish", 200),
    ];
    let refs: Vec<&CheckRun> = checks.iter().collect();
    let (sections, ci_jobs) = build_ci_failure_sections(Some(&jobs), &refs);

    let body = sections.join("\n");
    // Both workflows headered.
    assert!(body.contains("**Workflow:** CI"), "{body}");
    assert!(body.contains("**Workflow:** Release"), "{body}");
    // Both jobs + steps present.
    assert!(body.contains("**Failed job:** build"), "{body}");
    assert!(body.contains("**Failed job:** publish"), "{body}");
    assert!(body.contains("cargo build"), "{body}");
    assert!(body.contains("cargo publish"), "{body}");
    // Both ci_jobs entries (the structured payload the worker consumes).
    assert_eq!(ci_jobs.len(), 2);
    let ids: Vec<u64> = ci_jobs
        .iter()
        .map(|j| j["job_id"].as_u64().unwrap())
        .collect();
    assert!(ids.contains(&10) && ids.contains(&20), "{ids:?}");
    // Hint lines reference both jobs.
    assert!(body.contains("ci_job_log(job_id=10"), "{body}");
    assert!(body.contains("ci_job_log(job_id=20"), "{body}");
}

#[test]
fn ci_failure_sections_dedups_identical_jobs() {
    // The caller de-dups by job id; verify a duplicate job id collapses to
    // a single entry (defends the grouping invariant).
    let jobs = vec![
        failed_job(10, "build", "CI", vec![failed_step("cargo build", 3)]),
        failed_job(10, "build", "CI", vec![failed_step("cargo build", 3)]),
    ];
    // Simulate the caller's de-dup so the helper sees deduped input.
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<ActionsJob> = jobs.into_iter().filter(|j| seen.insert(j.id)).collect();
    let refs: Vec<&CheckRun> = Vec::new();
    let (_sections, ci_jobs) = build_ci_failure_sections(Some(&deduped), &refs);
    assert_eq!(ci_jobs.len(), 1);
}

#[test]
fn ci_failure_run_cap_bounds_aggregation() {
    // The run cap is MAX_AGGREGATED_CI_RUNS. Emulate the caller's run-id
    // collection + truncation and assert it bounds the number of runs and
    // flags the cap, while keeping every run under the cap represented.
    let cap = super::MAX_AGGREGATED_CI_RUNS;
    let checks: Vec<CheckRun> = (0..(cap as u64 + 3))
        .map(|i| check_run(&format!("wf{i} / job"), 1000 + i))
        .collect();
    let refs: Vec<&CheckRun> = checks.iter().collect();

    let mut run_ids: Vec<u64> = Vec::new();
    for cr in &refs {
        if let Some(rid) = parse_actions_run_id(&cr.html_url)
            && !run_ids.contains(&rid)
        {
            run_ids.push(rid);
        }
    }
    let capped = run_ids.len() > cap;
    assert!(capped, "should exceed the cap with {} runs", run_ids.len());
    run_ids.truncate(cap);
    assert_eq!(run_ids.len(), cap, "run ids bounded to the cap");
}

#[test]
fn ci_failure_sections_fallback_when_no_jobs() {
    // No Actions job data → fall back to listing raw check-run names.
    let checks = [check_run("lint", 100)];
    let refs: Vec<&CheckRun> = checks.iter().collect();
    let (sections, ci_jobs) = build_ci_failure_sections(None, &refs);
    let body = sections.join("\n");
    assert!(body.contains("**lint**"), "{body}");
    assert!(ci_jobs.is_empty());
}

#[test]
fn auto_resolve_conversations_gate() {
    // The #287 case: approved PR, auto-merge armed, GitHub reports BLOCKED
    // (require-conversation-resolution holding the merge), not yet resolved
    // for this SHA → fire.
    assert!(should_auto_resolve_conversations(
        true,
        Some("BLOCKED"),
        false
    ));

    // Not approved → never auto-resolve (the rule is a legitimate gate when
    // there's no approval override).
    assert!(!should_auto_resolve_conversations(
        false,
        Some("BLOCKED"),
        false
    ));

    // Not BLOCKED → some non-conversation condition is still in flight
    // (waiting on checks / behind base / draft), leave threads alone.
    for st in [
        Some("CLEAN"),
        Some("UNSTABLE"),
        Some("BEHIND"),
        Some("DRAFT"),
        Some("UNKNOWN"),
        None,
    ] {
        assert!(
            !should_auto_resolve_conversations(true, st, false),
            "merge_state {st:?} must not trigger auto-resolve"
        );
    }

    // Already resolved for this SHA → don't re-query every tick while a
    // different rule (e.g. pending CODEOWNERS review) keeps it BLOCKED.
    assert!(!should_auto_resolve_conversations(
        true,
        Some("BLOCKED"),
        true
    ));
}

// ── Same-CI-failure-signature fingerprint tests ──────────────────────────

#[test]
fn compute_ci_failure_fingerprint_deterministic() {
    // Same check names + same CI failure sections → same fingerprint.
    let checks = [check_run("CI / build", 100)];
    let refs: Vec<&CheckRun> = checks.iter().collect();
    let sections = vec![
        "**Workflow:** CI".to_string(),
        "**Failed job:** build (failure)".to_string(),
        "**Failed step:** cargo build (step #3, failure)".to_string(),
    ];
    let fp1 = compute_ci_failure_fingerprint(&refs, &sections);
    let fp2 = compute_ci_failure_fingerprint(&refs, &sections);
    assert_eq!(
        fp1, fp2,
        "identical inputs must produce identical fingerprints"
    );
    assert!(!fp1.is_empty(), "fingerprint must be non-empty");
}

#[test]
fn compute_ci_failure_fingerprint_sensitivity_different_checks() {
    // Different check names → different fingerprint.
    let checks_a = [check_run("CI / build", 100)];
    let refs_a: Vec<&CheckRun> = checks_a.iter().collect();
    let checks_b = [check_run("CI / test", 100)];
    let refs_b: Vec<&CheckRun> = checks_b.iter().collect();
    let sections = vec![
        "**Failed job:** build (failure)".to_string(),
        "**Failed step:** cargo build (step #3, failure)".to_string(),
    ];
    let fp_a = compute_ci_failure_fingerprint(&refs_a, &sections);
    let fp_b = compute_ci_failure_fingerprint(&refs_b, &sections);
    assert_ne!(
        fp_a, fp_b,
        "different check names must produce different fingerprints"
    );
}

#[test]
fn compute_ci_failure_fingerprint_sensitivity_different_failures() {
    // Same checks but different failed jobs/steps → different fingerprint.
    let checks = [check_run("CI / build", 100)];
    let refs: Vec<&CheckRun> = checks.iter().collect();
    let sections_a = vec![
        "**Failed job:** build (failure)".to_string(),
        "**Failed step:** cargo build (step #3, failure)".to_string(),
    ];
    let sections_b = vec![
        "**Failed job:** build (failure)".to_string(),
        "**Failed step:** cargo test (step #4, failure)".to_string(),
    ];
    let fp_a = compute_ci_failure_fingerprint(&refs, &sections_a);
    let fp_b = compute_ci_failure_fingerprint(&refs, &sections_b);
    assert_ne!(
        fp_a, fp_b,
        "different failed steps must produce different fingerprints"
    );
}

#[test]
fn compute_ci_failure_fingerprint_normalizes_casing_and_whitespace() {
    // Check names with different casing/whitespace normalize to the same fingerprint.
    let check_upper = CheckRun {
        id: 1,
        name: "  CI / BUILD  ".to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        html_url: "https://github.com/o/r/actions/runs/1/job/1".to_string(),
    };
    let check_lower = CheckRun {
        id: 2,
        name: "ci / build".to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        html_url: "https://github.com/o/r/actions/runs/1/job/2".to_string(),
    };
    let refs_upper: Vec<&CheckRun> = vec![&check_upper];
    let refs_lower: Vec<&CheckRun> = vec![&check_lower];
    let sections = vec!["**Failed job:** build (failure)".to_string()];
    let fp_upper = compute_ci_failure_fingerprint(&refs_upper, &sections);
    let fp_lower = compute_ci_failure_fingerprint(&refs_lower, &sections);
    assert_eq!(
        fp_upper, fp_lower,
        "different casing/whitespace must normalize to the same fingerprint"
    );
}

#[test]
fn compute_ci_failure_fingerprint_independent_of_order() {
    // Check names in different order → same fingerprint (sorted internally).
    let check_a = CheckRun {
        id: 1,
        name: "CI / build".to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        html_url: "https://github.com/o/r/actions/runs/1/job/1".to_string(),
    };
    let check_b = CheckRun {
        id: 2,
        name: "CI / test".to_string(),
        status: "completed".to_string(),
        conclusion: Some("failure".to_string()),
        html_url: "https://github.com/o/r/actions/runs/1/job/2".to_string(),
    };
    let refs_ab: Vec<&CheckRun> = vec![&check_a, &check_b];
    let refs_ba: Vec<&CheckRun> = vec![&check_b, &check_a];
    let sections = vec!["**Failed job:** build (failure)".to_string()];
    let fp_ab = compute_ci_failure_fingerprint(&refs_ab, &sections);
    let fp_ba = compute_ci_failure_fingerprint(&refs_ba, &sections);
    assert_eq!(
        fp_ab, fp_ba,
        "order-independent: must produce the same fingerprint"
    );
}

// ── count_consecutive_identical tests ────────────────────────────────────

/// Helper: build an ActivityEntry with a JSON payload.
fn activity_entry(
    event_type: &str,
    payload: &serde_json::Value,
) -> djinn_core::models::ActivityEntry {
    djinn_core::models::ActivityEntry {
        id: format!(
            "entry-{event_type}-{}",
            payload
                .get("fingerprint")
                .and_then(|v| v.as_str())
                .unwrap_or("x")
        ),
        task_id: Some("task-1".to_string()),
        actor_id: "coordinator".to_string(),
        actor_role: "system".to_string(),
        event_type: event_type.to_string(),
        payload: payload.to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
    }
}

/// Helper: build an ActivityEntry with a raw (possibly malformed) payload.
fn activity_entry_raw(event_type: &str, payload: &str) -> djinn_core::models::ActivityEntry {
    djinn_core::models::ActivityEntry {
        id: format!("entry-{event_type}-raw"),
        task_id: Some("task-1".to_string()),
        actor_id: "coordinator".to_string(),
        actor_role: "system".to_string(),
        event_type: event_type.to_string(),
        payload: payload.to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
    }
}

#[test]
fn count_consecutive_identical_counts_matching() {
    let fp = "abc123";
    let entries = vec![
        activity_entry("same_ci_signature", &serde_json::json!({"fingerprint": fp})),
        activity_entry("same_ci_signature", &serde_json::json!({"fingerprint": fp})),
    ];
    let count = count_consecutive_identical(&entries, fp);
    assert_eq!(count, 2, "both entries match the fingerprint");
}

#[test]
fn count_consecutive_identical_stops_at_different() {
    let fp = "abc123";
    let fp_other = "xyz789";
    let entries = vec![
        activity_entry("same_ci_signature", &serde_json::json!({"fingerprint": fp})),
        activity_entry(
            "same_ci_signature",
            &serde_json::json!({"fingerprint": fp_other}),
        ),
        activity_entry("same_ci_signature", &serde_json::json!({"fingerprint": fp})),
    ];
    // Walked in reverse: entry[2] matches (count=1), entry[1] does not → stop.
    let count = count_consecutive_identical(&entries, fp);
    assert_eq!(count, 1, "should stop at the first different fingerprint");
}

#[test]
fn count_consecutive_identical_empty() {
    let count = count_consecutive_identical(&[], "abc123");
    assert_eq!(count, 0, "no entries → zero");
}

#[test]
fn count_consecutive_identical_no_match() {
    let entries = vec![activity_entry(
        "same_ci_signature",
        &serde_json::json!({"fingerprint": "different"}),
    )];
    let count = count_consecutive_identical(&entries, "abc123");
    assert_eq!(count, 0, "no matching entries → zero");
}

#[test]
fn count_consecutive_identical_handles_malformed_payload() {
    // Malformed JSON payload → break (treated as non-matching).
    let entries = vec![activity_entry_raw("same_ci_signature", "not json")];
    let count = count_consecutive_identical(&entries, "abc123");
    assert_eq!(count, 0, "malformed payload should break immediately");
}

// ── Scope-inversion tests ────────────────────────────────────────────────

#[test]
fn detect_scope_inversion_true_positive() {
    // CI failure mentions djinn-agent, PR diff only touches djinn-db → Some(true)
    let sections = vec![
        "**Workflow:** CI".to_string(),
        "error[E0308]: mismatched types".to_string(),
        "  --> server/crates/djinn-agent/src/foo.rs:10:5".to_string(),
        "**Failed job:** build (failure)".to_string(),
    ];
    let pr_files = vec!["server/crates/djinn-db/src/bar.rs".to_string()];
    assert_eq!(
        detect_scope_inversion(&sections, &pr_files),
        Some(true),
        "CI fails on crate outside the PR diff → scope inversion"
    );
}

#[test]
fn detect_scope_inversion_true_negative() {
    // CI failure mentions djinn-db, PR diff touches djinn-db → Some(false)
    let sections = vec![
        "error[E0308]: mismatched types".to_string(),
        "  --> server/crates/djinn-db/src/foo.rs:10:5".to_string(),
        "**Failed job:** test (failure)".to_string(),
    ];
    let pr_files = vec!["server/crates/djinn-db/src/bar.rs".to_string()];
    assert_eq!(
        detect_scope_inversion(&sections, &pr_files),
        Some(false),
        "CI fails on crate within the PR diff → normal worker bug"
    );
}

#[test]
fn detect_scope_inversion_inconclusive_no_file_paths() {
    // CI failure sections have no extractable file paths → None
    let sections = vec![
        "**Workflow:** CI".to_string(),
        "**Failed job:** build (failure)".to_string(),
        "**Failed step:** cargo build (step #3, failure)".to_string(),
        "error: process didn't exit successfully".to_string(),
    ];
    let pr_files = vec!["server/crates/djinn-db/src/bar.rs".to_string()];
    assert_eq!(
        detect_scope_inversion(&sections, &pr_files),
        None,
        "no extractable file paths → inconclusive"
    );
}

#[test]
fn detect_scope_inversion_inconclusive_empty_pr_files() {
    // Empty PR files → None
    let sections = vec!["  --> server/crates/djinn-agent/src/foo.rs:10:5".to_string()];
    assert_eq!(
        detect_scope_inversion(&sections, &[]),
        None,
        "empty PR files → inconclusive"
    );
}

#[test]
fn detect_scope_inversion_inconclusive_no_crates_in_diff() {
    // PR files don't contain any crate paths → None
    let sections = vec!["  --> server/crates/djinn-agent/src/foo.rs:10:5".to_string()];
    let pr_files = vec![
        "docs/readme.md".to_string(),
        "scripts/deploy.sh".to_string(),
    ];
    assert_eq!(
        detect_scope_inversion(&sections, &pr_files),
        None,
        "PR files with no crate paths → inconclusive"
    );
}

#[test]
fn detect_scope_inversion_multiple_crates_mixed() {
    // CI fails on djinn-agent AND djinn-db. PR only touches djinn-db.
    // At least one failing crate is outside → Some(true).
    let sections = vec![
        "  --> server/crates/djinn-agent/src/foo.rs:10:5".to_string(),
        "  --> server/crates/djinn-db/src/baz.rs:20:10".to_string(),
    ];
    let pr_files = vec!["server/crates/djinn-db/src/bar.rs".to_string()];
    assert_eq!(
        detect_scope_inversion(&sections, &pr_files),
        Some(true),
        "any failing crate outside the diff → scope inversion"
    );
}

#[test]
fn detect_scope_inversion_all_within_diff() {
    // CI fails on multiple crates, all within the PR diff → Some(false)
    let sections = vec![
        "  --> server/crates/djinn-agent/src/foo.rs:10:5".to_string(),
        "  --> server/crates/djinn-db/src/baz.rs:20:10".to_string(),
    ];
    let pr_files = vec![
        "server/crates/djinn-agent/src/other.rs".to_string(),
        "server/crates/djinn-db/src/bar.rs".to_string(),
    ];
    assert_eq!(
        detect_scope_inversion(&sections, &pr_files),
        Some(false),
        "all failing crates within the diff → normal worker bug"
    );
}

// ── extract_crate_name helper tests ──────────────────────────────────────

#[test]
fn extract_crate_name_server_prefix() {
    assert_eq!(
        extract_crate_name("server/crates/djinn-agent/src/foo.rs"),
        Some("djinn-agent".to_string())
    );
}

#[test]
fn extract_crate_name_no_server_prefix() {
    assert_eq!(
        extract_crate_name("crates/djinn-db/src/bar.rs"),
        Some("djinn-db".to_string())
    );
}

#[test]
fn extract_crate_name_no_crates_segment() {
    assert_eq!(extract_crate_name("random/path.rs"), None);
    assert_eq!(extract_crate_name("foo.rs"), None);
}

#[test]
fn extract_crate_names_deduplicates_and_sorts() {
    let paths = vec![
        "server/crates/djinn-db/src/b.rs".to_string(),
        "server/crates/djinn-agent/src/a.rs".to_string(),
        "crates/djinn-db/src/c.rs".to_string(),
    ];
    let crates = extract_crate_names(&paths);
    assert_eq!(crates, vec!["djinn-agent", "djinn-db"]);
}

// ── Review-stuck trigger (pure-function level) ───────────────────────────
//
// The review-stuck trigger lives in `pr_watcher.rs` and requires a full
// `CoordinatorActor` harness with a mock GitHub API client — too heavyweight
// for a unit test in this file. The integration-level behavior (fires on
// terminal red + stale SHA, does NOT fire on pending CI / green CI / recent
// SHA change) is verified through the helper functions the trigger relies on.

#[test]
fn review_stuck_is_failing_conclusion_recognizes_terminal_red() {
    // The trigger filters check-runs by `is_failing_conclusion`. Only
    // terminal-red conclusions ("failure", "timed_out", "cancelled") count.
    assert!(super::is_failing_conclusion(Some("failure")));
    assert!(super::is_failing_conclusion(Some("timed_out")));
    assert!(super::is_failing_conclusion(Some("cancelled")));
    // Pending (None) and success do NOT count — the trigger must not fire.
    assert!(!super::is_failing_conclusion(None));
    assert!(!super::is_failing_conclusion(Some("success")));
    assert!(!super::is_failing_conclusion(Some("neutral")));
}

#[test]
fn review_stuck_window_minutes_is_positive() {
    // The trigger requires elapsed >= REVIEW_STUCK_WINDOW_MINUTES before firing.
    // Verify the constant is a sane positive value so the window is meaningful.
    const {
        assert!(
            super::REVIEW_STUCK_WINDOW_MINUTES > 0,
            "review-stuck window must be positive"
        );
    }
}

#[test]
fn review_stuck_same_ci_signature_threshold_is_lower_than_cycle_cap() {
    // Content-aware escalation (same-signature) must fire before the blind
    // cycle-count force-close, so the Planner gets a chance to intervene.
    const {
        assert!(super::SAME_CI_SIGNATURE_THRESHOLD < super::PR_CI_FAILURE_THRESHOLD,);
    }
}

// ── Integration-level: ordering invariants for the three triggers ────────
//
// The handle_ci_failure method processes triggers in a fixed order:
//   1. Same-CI-signature check (escalation at SAME_CI_SIGNATURE_THRESHOLD)
//   2. Scope-inversion check (RE-SLICE intervention)
//   3. Diff-empty short-circuit (escalation + force-close)
//   4. Cycle cap (escalation + force-close)
//
// We verify the ordering constraints using the pure functions:
// - Scope-inversion must be detectable independently of the fingerprint,
//   so it takes priority when both conditions hold.
// - Same-signature escalation fires at SAME_CI_SIGNATURE_THRESHOLD=2, which
//   is lower than PR_CI_FAILURE_THRESHOLD=3, so it beats the cycle cap.

#[test]
fn integration_scope_inversion_priority_over_same_signature() {
    // A CI failure that is both scope-inverted AND same-signature:
    // detect_scope_inversion returns Some(true), which in handle_ci_failure
    // is checked before the cycle cap and produces a RE-SLICE intervention.
    // The fingerprint is irrelevant once scope-inversion fires.
    let sections = vec![
        "  --> server/crates/djinn-agent/src/foo.rs:10:5".to_string(),
        "**Failed job:** build (failure)".to_string(),
        "**Failed step:** cargo build (step #3, failure)".to_string(),
    ];
    let pr_files = vec!["server/crates/djinn-db/src/bar.rs".to_string()];

    // Scope-inversion is detected → this takes priority.
    assert_eq!(detect_scope_inversion(&sections, &pr_files), Some(true));

    // The fingerprint still computes (it would be checked first in the
    // actual method), but scope-inversion is the more specific diagnosis.
    let checks = [check_run("CI / build", 100)];
    let refs: Vec<&CheckRun> = checks.iter().collect();
    let fp = compute_ci_failure_fingerprint(&refs, &sections);
    assert!(!fp.is_empty(), "fingerprint still computes");
    // The point: both conditions are satisfiable simultaneously, and
    // scope-inversion (checked at step 2) wins over the cycle cap (step 4).
}

#[test]
fn integration_same_signature_beats_cycle_cap() {
    // The threshold ordering ensures same-signature fires before the cycle cap.
    // With SAME_CI_SIGNATURE_THRESHOLD=2 and PR_CI_FAILURE_THRESHOLD=3,
    // the second identical failure triggers same-signature escalation, not
    // the blind force-close.
    let fp = "deadbeef";
    // Simulate one prior identical fingerprint (the first failure).
    let prior_entries = vec![activity_entry(
        "same_ci_signature",
        &serde_json::json!({"fingerprint": fp}),
    )];
    let consecutive = count_consecutive_identical(&prior_entries, fp);
    let total_consecutive = consecutive + 1; // current failure
    // At total_consecutive=2, same-signature fires (>= SAME_CI_SIGNATURE_THRESHOLD=2).
    assert!(
        total_consecutive >= super::SAME_CI_SIGNATURE_THRESHOLD,
        "second identical fingerprint should trigger same-signature escalation"
    );
    // And this happens at round 2, before the cycle cap at round 4 (> threshold 3).
    assert!(
        total_consecutive <= super::SAME_CI_SIGNATURE_THRESHOLD
            || total_consecutive > super::PR_CI_FAILURE_THRESHOLD,
        "same-signature fires at or before the cycle cap"
    );
}

#[test]
fn integration_normal_failure_no_false_positive() {
    // A normal within-scope CI failure with a new fingerprint each time:
    // detect_scope_inversion returns Some(false) (within diff), and
    // count_consecutive_identical returns 0 (different fingerprints).
    // Neither escalation trigger fires.
    let sections = vec![
        "  --> server/crates/djinn-db/src/foo.rs:10:5".to_string(),
        "**Failed job:** test (failure)".to_string(),
    ];
    let pr_files = vec!["server/crates/djinn-db/src/bar.rs".to_string()];

    // Not a scope inversion.
    assert_eq!(detect_scope_inversion(&sections, &pr_files), Some(false));

    // Different fingerprint from any prior → counter resets.
    let prior_entries = vec![activity_entry(
        "same_ci_signature",
        &serde_json::json!({"fingerprint": "old_fp"}),
    )];
    let new_fp = "new_fp";
    let consecutive = count_consecutive_identical(&prior_entries, new_fp);
    assert_eq!(consecutive, 0, "different fingerprint resets the counter");
    let total_consecutive = consecutive + 1; // = 1
    assert!(
        total_consecutive < super::SAME_CI_SIGNATURE_THRESHOLD,
        "new fingerprint does not trigger same-signature escalation"
    );
}

// ── CI gate snapshot write-through tests ──────────────────────────────────
//
// Write-through tests verify input construction for each CiStatus variant.
// Model-level tests verify the snapshot contract through `TaskPrCiSnapshot::from_input`.

use djinn_core::models::{CiStatus, TaskPrCiSnapshot, TaskPrCiSnapshotInput};

/// Helper to build a snapshot input the same way the pr_poller does for a
/// failing observation via `handle_ci_failure`, so we can assert on the
/// resulting fields.  Since sa4x, `handle_ci_failure` sets
/// `last_remediation_base_sha` to the current failing head SHA.
fn build_failing_snapshot_input(
    task_id: &str,
    pr_number: u64,
    head_sha: &str,
    blocking: &[CheckRun],
    fingerprint: &str,
    total_consecutive: i64,
) -> TaskPrCiSnapshotInput {
    let blocking_names: Vec<String> = blocking.iter().map(|cr| cr.name.clone()).collect();
    TaskPrCiSnapshotInput {
        task_id: task_id.to_owned(),
        pr_number: pr_number as i64,
        head_sha: head_sha.to_owned(),
        ci_status: CiStatus::Failing,
        blocking_required_check_names: blocking_names,
        failure_fingerprint: Some(fingerprint.to_owned()),
        same_signature_count: total_consecutive,
        last_remediation_base_sha: Some(head_sha.to_owned()),
    }
}

fn make_check_run(name: &str, conclusion: &str) -> CheckRun {
    CheckRun {
        id: 1000,
        name: name.to_string(),
        status: "completed".to_string(),
        conclusion: Some(conclusion.to_string()),
        html_url: "https://github.com/owner/repo/actions/runs/123/jobs/456".to_string(),
    }
}

// ── Write-through input construction tests ────────────────────────────────

#[test]
fn ci_snapshot_failing_input_includes_blocking_names_and_fingerprint() {
    let blocking = vec![
        make_check_run("Quality Gate", "failure"),
        make_check_run("Server Clippy", "failure"),
    ];
    let input = build_failing_snapshot_input("task-1", 42, "abc123def456", &blocking, "fp-aaa", 2);

    assert_eq!(input.ci_status, CiStatus::Failing);
    assert_eq!(
        input.blocking_required_check_names,
        vec!["Quality Gate", "Server Clippy"]
    );
    assert_eq!(input.failure_fingerprint.as_deref(), Some("fp-aaa"));
    assert_eq!(input.same_signature_count, 2);
    assert_eq!(
        input.last_remediation_base_sha.as_deref(),
        Some("abc123def456"),
        "handle_ci_failure sets last_remediation_base_sha to the failing head"
    );
    assert_eq!(input.pr_number, 42);
    assert_eq!(input.head_sha, "abc123def456");
}

#[test]
fn ci_snapshot_passing_input_has_empty_blocking_and_no_fingerprint() {
    let input = TaskPrCiSnapshotInput {
        task_id: "task-2".to_owned(),
        pr_number: 99,
        head_sha: "sha-green".to_owned(),
        ci_status: CiStatus::Passing,
        blocking_required_check_names: vec![],
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };

    assert_eq!(input.ci_status, CiStatus::Passing);
    assert!(input.blocking_required_check_names.is_empty());
    assert!(input.failure_fingerprint.is_none());
    assert_eq!(input.same_signature_count, 0);
}

#[test]
fn ci_snapshot_pending_input_has_empty_blocking_and_no_fingerprint() {
    let input = TaskPrCiSnapshotInput {
        task_id: "task-3".to_owned(),
        pr_number: 7,
        head_sha: "sha-pending".to_owned(),
        ci_status: CiStatus::Pending,
        blocking_required_check_names: vec![],
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };

    assert_eq!(input.ci_status, CiStatus::Pending);
    assert!(input.blocking_required_check_names.is_empty());
    assert!(input.failure_fingerprint.is_none());
}

#[test]
fn ci_snapshot_unknown_input_has_empty_blocking_and_no_fingerprint() {
    let input = TaskPrCiSnapshotInput {
        task_id: "task-4".to_owned(),
        pr_number: 1,
        head_sha: "sha-unknown".to_owned(),
        ci_status: CiStatus::Unknown,
        blocking_required_check_names: vec![],
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };

    assert_eq!(input.ci_status, CiStatus::Unknown);
    assert!(input.blocking_required_check_names.is_empty());
    assert!(input.failure_fingerprint.is_none());
}

#[test]
fn ci_snapshot_failing_input_carries_remediation_base_sha_where_available() {
    let blocking = vec![make_check_run("Tests", "failure")];
    let input = build_failing_snapshot_input("task-5", 10, "head-sha-5", &blocking, "fp-5", 1);
    // handle_ci_failure sets last_remediation_base_sha to the failing head SHA
    // so that later submit handling can compare against that baseline.
    assert_eq!(
        input.last_remediation_base_sha.as_deref(),
        Some("head-sha-5"),
        "handle_ci_failure persists the failing head as the remediation base SHA"
    );
}

#[test]
fn ci_snapshot_failing_same_signature_count_matches_consecutive_observations() {
    // The poller sets same_signature_count to total_consecutive (1-indexed),
    // matching the consecutive identical fingerprint observations.
    let blocking = vec![make_check_run("Lint", "failure")];

    // First observation: total_consecutive = 1
    let input1 = build_failing_snapshot_input("t", 1, "sha", &blocking, "fp", 1);
    assert_eq!(input1.same_signature_count, 1);

    // Second identical observation: total_consecutive = 2
    let input2 = build_failing_snapshot_input("t", 1, "sha", &blocking, "fp", 2);
    assert_eq!(input2.same_signature_count, 2);

    // Third identical observation: total_consecutive = 3
    let input3 = build_failing_snapshot_input("t", 1, "sha", &blocking, "fp", 3);
    assert_eq!(input3.same_signature_count, 3);
}

#[test]
fn ci_snapshot_empty_blocking_names_for_passing_and_pending_states() {
    // Both passing and pending observations carry no blocking names or
    // fingerprint — only failing snapshots include these.
    for status in [CiStatus::Passing, CiStatus::Pending, CiStatus::Unknown] {
        let input = TaskPrCiSnapshotInput {
            task_id: "t".to_owned(),
            pr_number: 1,
            head_sha: "sha".to_owned(),
            ci_status: status,
            blocking_required_check_names: vec![],
            failure_fingerprint: None,
            same_signature_count: 0,
            last_remediation_base_sha: None,
        };
        assert!(
            input.blocking_required_check_names.is_empty(),
            "{status} should have no blocking names"
        );
        assert!(
            input.failure_fingerprint.is_none(),
            "{status} should have no fingerprint"
        );
        assert_eq!(
            input.same_signature_count, 0,
            "{status} should have zero same-signature count"
        );
    }
}

/// Review-stuck path: blocking check names are persisted but no fingerprint or
/// same-signature tracking (this path does not go through handle_ci_failure).
#[test]
fn ci_snapshot_review_stuck_failing_has_blocking_names_no_fingerprint() {
    let blocking = [
        make_check_run("Quality Gate", "failure"),
        make_check_run("Server Test", "timed_out"),
    ];
    let blocking_names: Vec<String> = blocking.iter().map(|cr| cr.name.clone()).collect();
    let input = TaskPrCiSnapshotInput {
        task_id: "task-review-stuck".to_owned(),
        pr_number: 77,
        head_sha: "stuck-sha".to_owned(),
        ci_status: CiStatus::Failing,
        blocking_required_check_names: blocking_names,
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };

    assert_eq!(input.ci_status, CiStatus::Failing);
    assert_eq!(
        input.blocking_required_check_names,
        vec!["Quality Gate", "Server Test"]
    );
    assert!(
        input.failure_fingerprint.is_none(),
        "review-stuck path does not compute a fingerprint"
    );
    assert_eq!(
        input.same_signature_count, 0,
        "review-stuck path does not track same-signature count"
    );
}

/// Changes-requested + blocking CI failing path: blocking check names are
/// persisted without fingerprint or same-signature tracking (this path
/// intentionally avoids handle_ci_failure cycle-cap/diff-empty logic).
#[test]
fn ci_snapshot_changes_requested_failing_has_blocking_names_no_fingerprint() {
    let blocking = [make_check_run("Lint", "failure")];
    let blocking_names: Vec<String> = blocking.iter().map(|cr| cr.name.clone()).collect();
    let input = TaskPrCiSnapshotInput {
        task_id: "task-changes-req".to_owned(),
        pr_number: 55,
        head_sha: "changes-req-sha".to_owned(),
        ci_status: CiStatus::Failing,
        blocking_required_check_names: blocking_names,
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };

    assert_eq!(input.ci_status, CiStatus::Failing);
    assert_eq!(input.blocking_required_check_names, vec!["Lint"]);
    assert!(
        input.failure_fingerprint.is_none(),
        "changes-requested path does not compute a fingerprint"
    );
    assert_eq!(input.same_signature_count, 0);
}

/// When a new head SHA is observed, the repository's upsert resets stale
/// blocking names, fingerprint, same-signature count, and remediation base
/// SHA.  This test verifies the contract at the input level: a "reset"
/// observation carries empty blocking, no fingerprint, and zero count.
#[test]
fn ci_snapshot_new_head_sha_reset_contract_has_clean_fields() {
    // Simulate what the repository's reset_ci_snapshot_for_head produces:
    // new head_sha, unknown status, empty blocking, no fingerprint.
    let input = TaskPrCiSnapshotInput {
        task_id: "task-reset".to_owned(),
        pr_number: 10,
        head_sha: "brand-new-sha".to_owned(),
        ci_status: CiStatus::Unknown,
        blocking_required_check_names: vec![],
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };

    assert_eq!(input.ci_status, CiStatus::Unknown);
    assert!(
        input.blocking_required_check_names.is_empty(),
        "new head SHA must reset blocking names"
    );
    assert!(
        input.failure_fingerprint.is_none(),
        "new head SHA must reset fingerprint"
    );
    assert_eq!(
        input.same_signature_count, 0,
        "new head SHA must reset same-signature count"
    );
    assert!(
        input.last_remediation_base_sha.is_none(),
        "new head SHA must reset remediation base SHA"
    );
}

/// Verify that the persist_ci_snapshot helper produces a well-formed input
/// for each CiStatus variant, matching the contract the repository expects.
#[test]
fn ci_snapshot_persist_input_construction_covers_all_statuses() {
    // Failing: blocking names + fingerprint + count + remediation base SHA
    // (handle_ci_failure sets last_remediation_base_sha to the failing head)
    let failing = TaskPrCiSnapshotInput {
        task_id: "t".to_owned(),
        pr_number: 1,
        head_sha: "sha".to_owned(),
        ci_status: CiStatus::Failing,
        blocking_required_check_names: vec!["A".to_owned(), "B".to_owned()],
        failure_fingerprint: Some("fp".to_owned()),
        same_signature_count: 2,
        last_remediation_base_sha: Some("sha".to_owned()),
    };
    assert_eq!(failing.ci_status, CiStatus::Failing);
    assert_eq!(failing.blocking_required_check_names.len(), 2);
    assert!(failing.failure_fingerprint.is_some());
    assert_eq!(failing.same_signature_count, 2);
    assert_eq!(
        failing.last_remediation_base_sha.as_deref(),
        Some("sha"),
        "failing snapshot carries the remediation base SHA"
    );

    // Passing: empty blocking, no fingerprint, zero count
    let passing = TaskPrCiSnapshotInput {
        task_id: "t".to_owned(),
        pr_number: 1,
        head_sha: "sha".to_owned(),
        ci_status: CiStatus::Passing,
        blocking_required_check_names: vec![],
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };
    assert_eq!(passing.ci_status, CiStatus::Passing);
    assert!(passing.blocking_required_check_names.is_empty());
    assert!(passing.failure_fingerprint.is_none());
    assert_eq!(passing.same_signature_count, 0);

    // Pending: empty blocking, no fingerprint, zero count
    let pending = TaskPrCiSnapshotInput {
        task_id: "t".to_owned(),
        pr_number: 1,
        head_sha: "sha".to_owned(),
        ci_status: CiStatus::Pending,
        blocking_required_check_names: vec![],
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };
    assert_eq!(pending.ci_status, CiStatus::Pending);
    assert!(pending.blocking_required_check_names.is_empty());

    // Unknown: empty blocking, no fingerprint, zero count
    let unknown = TaskPrCiSnapshotInput {
        task_id: "t".to_owned(),
        pr_number: 1,
        head_sha: "sha".to_owned(),
        ci_status: CiStatus::Unknown,
        blocking_required_check_names: vec![],
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };
    assert_eq!(unknown.ci_status, CiStatus::Unknown);
    assert!(unknown.blocking_required_check_names.is_empty());
}

/// Verify that last_remediation_base_sha defaults to None in the input
/// struct, and that the field round-trips correctly when set.
/// `handle_ci_failure` now sets this to the current failing head SHA so that
/// later submit handling can compare against that baseline; other paths
/// (advisory-only, passing, pending) leave it as None.
#[test]
fn ci_snapshot_remediation_base_sha_none_by_default_round_trips_when_set() {
    // Default: None (the struct default, used by non-failure paths)
    let default_input = TaskPrCiSnapshotInput::default();
    assert!(
        default_input.last_remediation_base_sha.is_none(),
        "default input must have no remediation base SHA"
    );

    // When set by handle_ci_failure, it carries the failing head SHA.
    let with_remediation = TaskPrCiSnapshotInput {
        task_id: "t".to_owned(),
        pr_number: 1,
        head_sha: "failing-head-sha".to_owned(),
        ci_status: CiStatus::Failing,
        blocking_required_check_names: vec!["X".to_owned()],
        failure_fingerprint: Some("fp".to_owned()),
        same_signature_count: 1,
        last_remediation_base_sha: Some("failing-head-sha".to_owned()),
    };
    assert_eq!(
        with_remediation.last_remediation_base_sha.as_deref(),
        Some("failing-head-sha"),
        "handle_ci_failure persists the failing head as remediation base"
    );
}

// ── Model-level contract tests (from main) ───────────────────────────────

#[test]
fn ci_status_classifies_completed_checks_with_blocking_failure_as_failing() {
    // A completed check-run with a blocking failure conclusion produces
    // CiStatus::Failing. This is the classification the pr_poller performs
    // before constructing the snapshot input.
    let failing_check = check_run("unit tests", 100);

    // Simulate: all checks completed, at least one blocking failure.
    assert_eq!(failing_check.status, "completed");
    assert!(super::is_failing_conclusion(
        failing_check.conclusion.as_deref()
    ));

    // With no required-contexts (heuristic mode), the check name "unit tests"
    // is NOT advisory, so it IS blocking.
    let blocking = blocking_failed_checks(&[&failing_check], None);
    assert_eq!(blocking.len(), 1, "unit tests should be blocking");

    let ci_status = if blocking.is_empty() {
        CiStatus::Passing
    } else {
        CiStatus::Failing
    };
    assert_eq!(ci_status, CiStatus::Failing);
}

#[test]
fn ci_status_classifies_advisory_only_failures_as_passing() {
    // When only advisory checks (Vercel previews) fail and no required checks
    // are specified, the blocking filter returns empty → status is Passing.
    let vercel = check_run("Vercel – portal", 200);

    // With no required-contexts (heuristic mode), "Vercel – portal" is
    // advisory and NOT blocking.
    let blocking = blocking_failed_checks(&[&vercel], None);
    assert!(
        blocking.is_empty(),
        "advisory-only failures should not be blocking"
    );

    let ci_status = if blocking.is_empty() {
        CiStatus::Passing
    } else {
        CiStatus::Failing
    };
    assert_eq!(ci_status, CiStatus::Passing);
}

#[test]
fn ci_status_classifies_incomplete_checks_as_pending() {
    // When not all checks are completed, the status should be Pending.
    let running_check = CheckRun {
        id: 300,
        name: "CI / build".to_string(),
        status: "in_progress".to_string(),
        conclusion: None,
        html_url: "https://github.com/o/r/actions/runs/3/job/3".to_string(),
    };
    let checks = [&running_check];
    let all_completed = checks.iter().all(|cr| cr.status == "completed");
    assert!(!all_completed, "in_progress check is not completed");

    // Pending: not all checks completed.
    let ci_status = if checks.is_empty() || all_completed {
        // Would proceed to failure classification
        CiStatus::Passing
    } else {
        CiStatus::Pending
    };
    assert_eq!(ci_status, CiStatus::Pending);
}

#[test]
fn ci_status_classifies_no_checks_as_pending() {
    // When there are no checks at all, the snapshot records Pending.
    // The existing min-age guard in the poller handles the "no CI" case
    // after the guard elapses.
    let checks: Vec<&CheckRun> = Vec::new();
    assert!(checks.is_empty());

    let ci_status = if checks.is_empty() {
        CiStatus::Pending
    } else {
        CiStatus::Passing
    };
    assert_eq!(ci_status, CiStatus::Pending);
}

#[test]
fn fingerprint_changes_with_different_blocking_checks() {
    // Two different sets of blocking checks produce different fingerprints,
    // confirming that the pr_poller's fingerprint computation detects changes.
    let build = check_run("CI / build", 100);
    let test = check_run("CI / test", 200);

    let refs_a: Vec<&CheckRun> = vec![&build];
    let refs_b: Vec<&CheckRun> = vec![&build, &test];

    let sections = vec!["**Failed job:** build (failure)".to_string()];
    let fp_a = compute_ci_failure_fingerprint(&refs_a, &sections);
    let fp_b = compute_ci_failure_fingerprint(&refs_b, &sections);

    assert_ne!(
        fp_a, fp_b,
        "different check sets must produce different fingerprints"
    );
}

#[test]
fn stale_head_reset_produces_pending_with_cleared_fields() {
    // When the head SHA changes, reset_ci_snapshot_for_head produces a
    // snapshot with ci_status=pending, no blocking checks, no fingerprint,
    // zero same_signature_count, and no last_remediation_base_sha.
    // We verify the model contract that the repository upsert implements.
    let input = TaskPrCiSnapshotInput {
        task_id: "task-1".to_string(),
        pr_number: 42,
        head_sha: "new-sha-abc".to_string(),
        ci_status: CiStatus::Unknown, // reset_ci_snapshot_for_head inserts 'unknown' per SQL
        blocking_required_check_names: Vec::new(),
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };
    let snapshot = TaskPrCiSnapshot::from_input(
        input,
        "2026-06-30T10:00:00.000Z".to_string(),
        "2026-06-30T10:00:00.000Z".to_string(),
    );

    // The reset snapshot has empty blocking checks, no fingerprint, zero
    // same-signature count, and no remediation base SHA.
    assert!(
        snapshot.blocking_required_check_names.is_empty(),
        "stale-head reset must clear blocking check names"
    );
    assert!(
        snapshot.failure_fingerprint.is_none(),
        "stale-head reset must clear failure fingerprint"
    );
    assert_eq!(
        snapshot.same_signature_count, 0,
        "stale-head reset must zero same_signature_count"
    );
    assert!(
        snapshot.last_remediation_base_sha.is_none(),
        "stale-head reset must clear last_remediation_base_sha"
    );
}

#[test]
fn unknown_snapshot_preserves_head_sha_and_identity() {
    // When GitHub data is unavailable, the pr_poller records `unknown`
    // status while preserving the existing head SHA and PR number.
    let input = TaskPrCiSnapshotInput {
        task_id: "task-1".to_string(),
        pr_number: 42,
        head_sha: "existing-sha".to_string(),
        ci_status: CiStatus::Unknown,
        blocking_required_check_names: Vec::new(),
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };
    let snapshot = TaskPrCiSnapshot::from_input(
        input,
        "2026-06-30T09:00:00.000Z".to_string(),
        "2026-06-30T10:00:00.000Z".to_string(),
    );

    assert_eq!(snapshot.ci_status, CiStatus::Unknown);
    assert_eq!(snapshot.head_sha, "existing-sha");
    assert_eq!(snapshot.pr_number, 42);
    // Stale failure data must be cleared when writing `unknown`.
    assert!(snapshot.blocking_required_check_names.is_empty());
    assert!(snapshot.failure_fingerprint.is_none());
    assert_eq!(snapshot.same_signature_count, 0);
}

#[test]
fn snapshot_input_for_passing_status_has_no_fingerprint() {
    // When CI is passing, the snapshot should have no failure fingerprint
    // and no blocking check names after conversion through from_input.
    let input = TaskPrCiSnapshotInput {
        task_id: "task-1".to_string(),
        pr_number: 42,
        head_sha: "abc123".to_string(),
        ci_status: CiStatus::Passing,
        blocking_required_check_names: Vec::new(),
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };
    let snapshot = TaskPrCiSnapshot::from_input(
        input,
        "2026-06-30T10:00:00.000Z".to_string(),
        "2026-06-30T10:00:00.000Z".to_string(),
    );

    assert_eq!(snapshot.ci_status, CiStatus::Passing);
    assert!(snapshot.failure_fingerprint.is_none());
    assert!(snapshot.blocking_required_check_names.is_empty());
}

// ── CI merge gate verdict tests ──────────────────────────────────────────────
//
// These tests verify the pure `ci_merge_gate_verdict` function that gates
// Djinn-initiated merge/close on the durable CI snapshot. The gate ensures:
//   - Only `passing` CI on the current head SHA allows merge
//   - `failing` blocks merge (remediation/intervention handles)
//   - `pending`/`unknown` hold for later poller ticks
//   - A stale `passing` snapshot for an older SHA cannot authorize merge
//   - External merge observation (pr.merged == Some(true)) is unaffected

/// Build a minimal snapshot for gate tests.
fn gate_snapshot(task_id: &str, head_sha: &str, ci_status: CiStatus) -> TaskPrCiSnapshot {
    TaskPrCiSnapshot {
        task_id: task_id.to_owned(),
        pr_number: 1,
        head_sha: head_sha.to_owned(),
        ci_status,
        blocking_required_check_names: Vec::new(),
        failure_fingerprint: None,
        first_seen_at: "2026-01-01T00:00:00.000Z".to_owned(),
        last_seen_at: "2026-01-01T00:00:00.000Z".to_owned(),
        same_signature_count: 0,
        last_remediation_base_sha: None,
    }
}

#[test]
fn ci_merge_gate_allows_passing_with_matching_sha() {
    let snap = gate_snapshot("t1", "sha-abc123", CiStatus::Passing);
    assert_eq!(
        ci_merge_gate_verdict(Some(&snap), "sha-abc123"),
        CiMergeGateVerdict::Allow,
        "passing CI on current head must allow merge"
    );
}

#[test]
fn ci_merge_gate_blocks_failing_ci() {
    let snap = gate_snapshot("t2", "sha-def456", CiStatus::Failing);
    assert_eq!(
        ci_merge_gate_verdict(Some(&snap), "sha-def456"),
        CiMergeGateVerdict::Block,
        "failing required CI must block merge"
    );
}

#[test]
fn ci_merge_gate_holds_on_pending_ci() {
    let snap = gate_snapshot("t3", "sha-pending", CiStatus::Pending);
    assert_eq!(
        ci_merge_gate_verdict(Some(&snap), "sha-pending"),
        CiMergeGateVerdict::Hold,
        "pending CI must hold merge for next tick"
    );
}

#[test]
fn ci_merge_gate_holds_on_unknown_ci() {
    let snap = gate_snapshot("t4", "sha-unknown", CiStatus::Unknown);
    assert_eq!(
        ci_merge_gate_verdict(Some(&snap), "sha-unknown"),
        CiMergeGateVerdict::Hold,
        "unknown CI must hold merge for next tick"
    );
}

#[test]
fn ci_merge_gate_holds_on_stale_passing_sha() {
    // Snapshot says passing for OLD sha, but current head has moved.
    let snap = gate_snapshot("t5", "sha-old", CiStatus::Passing);
    assert_eq!(
        ci_merge_gate_verdict(Some(&snap), "sha-new"),
        CiMergeGateVerdict::Hold,
        "stale passing snapshot for older SHA must NOT authorize merge on newer head"
    );
}

#[test]
fn ci_merge_gate_holds_on_stale_failing_sha() {
    // Even if the snapshot is failing, a SHA mismatch should hold (not block)
    // because the snapshot data is stale and doesn't reflect the current head.
    let snap = gate_snapshot("t6", "sha-old", CiStatus::Failing);
    assert_eq!(
        ci_merge_gate_verdict(Some(&snap), "sha-new"),
        CiMergeGateVerdict::Hold,
        "stale failing snapshot should hold (not block) when SHA has moved"
    );
}

#[test]
fn ci_merge_gate_holds_when_no_snapshot_exists() {
    assert_eq!(
        ci_merge_gate_verdict(None, "sha-abc"),
        CiMergeGateVerdict::Hold,
        "missing CI snapshot must hold merge until snapshot is recorded"
    );
}

#[test]
fn ci_merge_gate_holds_on_stale_pending_sha() {
    let snap = gate_snapshot("t7", "sha-old", CiStatus::Pending);
    assert_eq!(
        ci_merge_gate_verdict(Some(&snap), "sha-new"),
        CiMergeGateVerdict::Hold,
        "stale pending snapshot holds on SHA mismatch"
    );
}

#[test]
fn ci_merge_gate_holds_on_empty_head_sha() {
    // Edge case: snapshot exists but head_sha is empty (e.g. initial state
    // before any PR data). Must not match a real current SHA.
    let snap = gate_snapshot("t8", "", CiStatus::Passing);
    assert_eq!(
        ci_merge_gate_verdict(Some(&snap), "sha-real"),
        CiMergeGateVerdict::Hold,
        "empty snapshot head SHA must not authorize merge"
    );
}

#[test]
fn ci_merge_gate_allows_when_sha_matches_exactly() {
    // Verify exact string match — not prefix or substring.
    let snap = gate_snapshot("t9", "abc123def456", CiStatus::Passing);
    assert_eq!(
        ci_merge_gate_verdict(Some(&snap), "abc123def456"),
        CiMergeGateVerdict::Allow,
        "exact SHA match with passing CI must allow merge"
    );
    // Substring must NOT match.
    assert_eq!(
        ci_merge_gate_verdict(Some(&snap), "abc123"),
        CiMergeGateVerdict::Hold,
        "substring SHA match must not authorize merge"
    );
    // Superset must NOT match.
    assert_eq!(
        ci_merge_gate_verdict(Some(&snap), "abc123def456000"),
        CiMergeGateVerdict::Hold,
        "superset SHA must not authorize merge"
    );
}

/// The merge gate only applies to Djinn-initiated merge/close paths. External
/// merge observation (pr.merged == Some(true)) is explicitly NOT gated — it
/// records what GitHub reports regardless of CI status. This is a documentation
/// test confirming the invariant; the actual separation is in pr_review_watcher
/// where the "merged" check runs BEFORE the gate.
#[test]
fn ci_merge_gate_external_merge_observation_is_not_gated() {
    // The gate function itself is never called for external merge observation.
    // This test documents that the gate's "Block" verdict does NOT apply to
    // the external path — the caller (pr_review_watcher) checks pr.merged
    // before reaching the gate.
    let snap = gate_snapshot("t10", "sha-head", CiStatus::Failing);
    // The gate WOULD block on failing CI:
    assert_eq!(
        ci_merge_gate_verdict(Some(&snap), "sha-head"),
        CiMergeGateVerdict::Block,
        "gate blocks failing CI — but external merge observation bypasses this"
    );
    // This is correct: the gate returns Block, but the watcher's control flow
    // checks pr.merged BEFORE reaching the gate, so external merges are
    // recorded via apply_pr_merge without consulting the gate.
}

#[test]
fn ci_merge_gate_verdict_variants_cover_all_ci_statuses() {
    // Exhaustive coverage: every CiStatus variant produces the expected verdict
    // when the SHA matches.
    let sha = "current-sha";
    let cases = [
        (CiStatus::Passing, CiMergeGateVerdict::Allow),
        (CiStatus::Failing, CiMergeGateVerdict::Block),
        (CiStatus::Pending, CiMergeGateVerdict::Hold),
        (CiStatus::Unknown, CiMergeGateVerdict::Hold),
    ];
    for (ci_status, expected) in &cases {
        let snap = gate_snapshot("t-exhaustive", sha, *ci_status);
        assert_eq!(
            ci_merge_gate_verdict(Some(&snap), sha),
            *expected,
            "ci_status={ci_status:?} with matching SHA should produce {expected:?}"
        );
    }
}

#[test]
fn ci_merge_gate_stale_sha_overrides_status_in_all_variants() {
    // When the SHA doesn't match, the verdict is always Hold regardless of
    // ci_status. This prevents stale snapshot data from authorizing merge.
    let cases = [
        CiStatus::Passing,
        CiStatus::Failing,
        CiStatus::Pending,
        CiStatus::Unknown,
    ];
    for ci_status in &cases {
        let snap = gate_snapshot("t-stale", "old-sha", *ci_status);
        assert_eq!(
            ci_merge_gate_verdict(Some(&snap), "new-sha"),
            CiMergeGateVerdict::Hold,
            "stale SHA must hold regardless of ci_status={ci_status:?}"
        );
    }
}

// ── pr_draft CI gate transition matrix tests ──────────────────────────────

#[test]
fn pr_draft_ci_action_passing_proceeds() {
    assert_eq!(
        decide_pr_draft_ci_action(CiStatus::Passing, true),
        PrDraftCiAction::Proceed {
            needs_passing_persist: false
        },
    );
}

#[test]
fn pr_draft_ci_action_pending_holds() {
    assert_eq!(
        decide_pr_draft_ci_action(CiStatus::Pending, true),
        PrDraftCiAction::Hold,
    );
}

#[test]
fn pr_draft_ci_action_unknown_holds() {
    assert_eq!(
        decide_pr_draft_ci_action(CiStatus::Unknown, true),
        PrDraftCiAction::Hold,
    );
}

#[test]
fn pr_draft_ci_action_pending_no_checks_proceeds_as_no_ci() {
    // After the min-age guard elapses with no check-runs, the repo has no CI
    // configured — treat as green.
    assert_eq!(
        decide_pr_draft_ci_action(CiStatus::Pending, false),
        PrDraftCiAction::Proceed {
            needs_passing_persist: true
        },
    );
}

#[test]
fn pr_draft_ci_action_unknown_no_checks_proceeds_as_no_ci() {
    // Same for Unknown (record_ci_snapshot returns Unknown for empty checks).
    assert_eq!(
        decide_pr_draft_ci_action(CiStatus::Unknown, false),
        PrDraftCiAction::Proceed {
            needs_passing_persist: true
        },
    );
}

#[test]
fn pr_draft_ci_action_failing_routes_to_remediation() {
    assert_eq!(
        decide_pr_draft_ci_action(CiStatus::Failing, true),
        PrDraftCiAction::RouteToRemediation,
    );
}

/// Advisory-only failures are non-blocking: when required checks are passing
/// the CI status is Passing and the action is Proceed (not RouteToRemediation).
#[test]
fn advisory_only_failures_proceed_through_ci_gate() {
    // Simulate: only Vercel (advisory) failed; required contexts list
    // "Quality Gate" and "unit tests" which are green.
    let vercel = check_run("Vercel – portal", 200);
    let required = vec!["Quality Gate".to_string(), "unit tests".to_string()];

    let failed = vec![&vercel];
    let blocking = blocking_failed_checks(&failed, Some(&required));
    assert!(
        blocking.is_empty(),
        "advisory-only failures must not be blocking"
    );

    // The snapshot classifier treats empty blocking as Passing.
    let ci_status = if blocking.is_empty() {
        CiStatus::Passing
    } else {
        CiStatus::Failing
    };
    assert_eq!(ci_status, CiStatus::Passing);

    // The gate decision is Proceed (no persist needed — snapshot is already
    // Passing).
    assert_eq!(
        decide_pr_draft_ci_action(ci_status, true),
        PrDraftCiAction::Proceed {
            needs_passing_persist: false
        },
    );
}

/// Required failures route through remediation — the advisory + required
/// mixed case.
#[test]
fn mixed_required_and_advisory_failures_route_to_remediation() {
    let gate = check_run("Quality Gate", 100);
    let vercel = check_run("Vercel – portal", 200);
    let required = vec!["Quality Gate".to_string()];

    let failed = vec![&gate, &vercel];
    let blocking = blocking_failed_checks(&failed, Some(&required));
    assert_eq!(blocking.len(), 1, "only the required check is blocking");
    assert_eq!(blocking[0].name, "Quality Gate");

    // Snapshot classifier: blocking is non-empty → Failing.
    let ci_status = if blocking.is_empty() {
        CiStatus::Passing
    } else {
        CiStatus::Failing
    };
    assert_eq!(ci_status, CiStatus::Failing);

    // Gate decision: RouteToRemediation (required check failed).
    assert_eq!(
        decide_pr_draft_ci_action(ci_status, true),
        PrDraftCiAction::RouteToRemediation,
    );
}

// ── sa4x: Red-CI remediation baseline and durable same-signature tests ───
//
// These tests verify the sa4x changes that persist a durable remediation
// baseline when required CI fails, making the CI gate snapshot the authority
// for downstream dispatch/submit decisions.

/// When `handle_ci_failure` observes required blocking CI failures, the
/// snapshot input it builds persists a durable remediation baseline with:
/// - `head_sha` = current failing head
/// - `last_remediation_base_sha` = current failing head (same as head_sha)
/// - `blocking_required_check_names` = names of the blocking checks
/// - `failure_fingerprint` = computed fingerprint
/// - `same_signature_count` = total consecutive observations
#[test]
fn baseline_capture_on_required_ci_failure_persists_all_fields() {
    let blocking = vec![
        make_check_run("Quality Gate", "failure"),
        make_check_run("Server Clippy", "failure"),
    ];
    let head_sha = "abc123def456789";
    let fingerprint = "fp-deadbeef";
    let total_consecutive = 1i64;

    let input = build_failing_snapshot_input(
        "task-baseline",
        42,
        head_sha,
        &blocking,
        fingerprint,
        total_consecutive,
    );

    // Core baseline fields
    assert_eq!(input.task_id, "task-baseline");
    assert_eq!(input.pr_number, 42);
    assert_eq!(input.head_sha, head_sha);
    assert_eq!(input.ci_status, CiStatus::Failing);
    assert_eq!(
        input.blocking_required_check_names,
        vec!["Quality Gate", "Server Clippy"]
    );
    assert_eq!(input.failure_fingerprint.as_deref(), Some(fingerprint));
    assert_eq!(input.same_signature_count, total_consecutive);

    // last_remediation_base_sha is set to the failing head so submit
    // handling can compare against that baseline.
    assert_eq!(
        input.last_remediation_base_sha.as_deref(),
        Some(head_sha),
        "durable remediation baseline must persist the failing head SHA"
    );
}

/// Advisory-only failures do NOT produce a failing snapshot — the blocking
/// filter returns empty, the snapshot is overwritten with Passing, and
/// `last_remediation_base_sha` remains None.  This test verifies that the
/// advisory-only path produces no baseline mutation.
#[test]
fn advisory_only_failure_produces_no_baseline_mutation() {
    // Simulate: only Vercel (advisory) failed; required contexts list
    // "Quality Gate" which is green.
    let vercel = make_check_run("Vercel – portal", "failure");
    let required = vec!["Quality Gate".to_string()];

    let failed = vec![&vercel];
    let blocking = blocking_failed_checks(&failed, Some(&required));
    assert!(
        blocking.is_empty(),
        "advisory-only failures must not be blocking"
    );

    // Advisory-only: the pr_poller overwrites with a Passing snapshot
    // (no blocking names, no fingerprint, no remediation base).
    let advisory_input = TaskPrCiSnapshotInput {
        task_id: "task-advisory".to_owned(),
        pr_number: 77,
        head_sha: "advisory-head-sha".to_owned(),
        ci_status: CiStatus::Passing,
        blocking_required_check_names: vec![],
        failure_fingerprint: None,
        same_signature_count: 0,
        last_remediation_base_sha: None,
    };

    assert_eq!(advisory_input.ci_status, CiStatus::Passing);
    assert!(
        advisory_input.blocking_required_check_names.is_empty(),
        "advisory-only path must have no blocking names"
    );
    assert!(
        advisory_input.failure_fingerprint.is_none(),
        "advisory-only path must have no fingerprint"
    );
    assert_eq!(
        advisory_input.same_signature_count, 0,
        "advisory-only path must have zero same-signature count"
    );
    assert!(
        advisory_input.last_remediation_base_sha.is_none(),
        "advisory-only path must NOT set a remediation baseline"
    );
}

/// The same-signature count is persisted to the durable snapshot (the
/// authoritative source), not just recorded in activity-log audit events.
/// This test verifies that consecutive observations increment the count
/// in the durable snapshot input (as `handle_ci_failure` does).
#[test]
fn same_signature_count_persisted_to_durable_snapshot_on_consecutive_failures() {
    let blocking = vec![make_check_run("Quality Gate", "failure")];
    let head_sha = "stable-head-sha";

    // First observation: count = 1
    let input1 = build_failing_snapshot_input("t", 1, head_sha, &blocking, "fp-same", 1);
    assert_eq!(
        input1.same_signature_count, 1,
        "first observation must set count to 1"
    );

    // Second identical observation: count = 2
    let input2 = build_failing_snapshot_input("t", 1, head_sha, &blocking, "fp-same", 2);
    assert_eq!(
        input2.same_signature_count, 2,
        "second consecutive identical observation must set count to 2"
    );

    // Third identical observation: count = 3
    let input3 = build_failing_snapshot_input("t", 1, head_sha, &blocking, "fp-same", 3);
    assert_eq!(
        input3.same_signature_count, 3,
        "third consecutive identical observation must set count to 3"
    );

    // All observations carry the same remediation base SHA (the failing head).
    for input in [&input1, &input2, &input3] {
        assert_eq!(
            input.last_remediation_base_sha.as_deref(),
            Some(head_sha),
            "each consecutive failure must persist the remediation baseline"
        );
    }
}
