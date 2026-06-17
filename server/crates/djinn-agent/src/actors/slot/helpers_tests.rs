use super::MERGE_CONFLICT_PREFIX;
use super::helpers::*;
use crate::test_helpers::{
    agent_context_from_db, create_test_db, create_test_epic, create_test_project, create_test_task,
};
use djinn_core::commands::CommandSpec;
use djinn_db::TaskRepository;
use tokio_util::sync::CancellationToken;

#[test]
fn parse_conflict_metadata_patterns() {
    let raw = r#"{"merge_target":"main","base_branch":"feature","conflicting_files":["a.rs"]}"#;
    assert!(parse_conflict_metadata(&format!("{MERGE_CONFLICT_PREFIX}{raw}")).is_some());
    assert!(parse_conflict_metadata(raw).is_none());
    assert!(parse_conflict_metadata(&format!("{MERGE_CONFLICT_PREFIX}{{not-json")).is_none());
}

#[test]
fn provider_helpers_cover_branches() {
    use djinn_provider::provider::{AuthMethod, FormatFamily};

    assert_eq!(
        format_family_for_provider("anthropic", "claude-3"),
        FormatFamily::Anthropic
    );
    assert_eq!(
        format_family_for_provider("google", "gemini-2.0"),
        FormatFamily::Google
    );
    assert_eq!(
        format_family_for_provider("vertex-ai", "gemini-2.0"),
        FormatFamily::Google
    );
    assert_eq!(
        format_family_for_provider("foo", "codex-mini"),
        FormatFamily::OpenAIResponses
    );
    assert_eq!(
        format_family_for_provider("openai", "gpt-4o"),
        FormatFamily::OpenAI
    );
    // GPT-5.x and o-series on native OpenAI → Responses API
    assert_eq!(
        format_family_for_provider("openai", "gpt-5.4"),
        FormatFamily::OpenAIResponses
    );
    assert_eq!(
        format_family_for_provider("openai", "o3"),
        FormatFamily::OpenAIResponses
    );
    assert_eq!(
        format_family_for_provider("openai", "o4-mini"),
        FormatFamily::OpenAIResponses
    );
    // Third-party OpenAI-compatible providers stay on Chat Completions
    assert_eq!(
        format_family_for_provider("fireworks", "gpt-5.4"),
        FormatFamily::OpenAI
    );
    // Xiaomi MiMo Token Plan is Anthropic-compatible; the dotted model id
    // (`mimo-v2.5-pro`) must not perturb routing.
    assert_eq!(
        format_family_for_provider("xiaomi-mimo", "mimo-v2.5-pro"),
        FormatFamily::Anthropic
    );

    let anthropic = capabilities_for_provider("anthropic");
    assert!(anthropic.streaming);
    assert_eq!(anthropic.max_tokens_default, Some(64_000));

    // Xiaomi MiMo gets Anthropic-style caps (streaming + default max_tokens).
    let mimo = capabilities_for_provider("xiaomi-mimo");
    assert!(mimo.streaming);
    assert_eq!(mimo.max_tokens_default, Some(64_000));
    // ...and Bearer auth (NOT the Anthropic-native x-api-key path).
    assert!(matches!(
        auth_method_for_provider("xiaomi-mimo", "tp-abc"),
        AuthMethod::BearerToken(v) if v == "tp-abc"
    ));

    let synthetic = capabilities_for_provider("synthetic-provider");
    assert!(!synthetic.streaming);
    assert_eq!(synthetic.max_tokens_default, None);

    let local = capabilities_for_provider("local-provider");
    assert!(!local.streaming);

    let default_caps = capabilities_for_provider("openai");
    let expected_default = djinn_provider::provider::ProviderCapabilities::default();
    assert_eq!(default_caps.streaming, expected_default.streaming);
    assert_eq!(
        default_caps.max_tokens_default,
        expected_default.max_tokens_default
    );

    match auth_method_for_provider("anthropic", "k") {
        AuthMethod::ApiKeyHeader { header, key } => {
            assert_eq!(header, "x-api-key");
            assert_eq!(key, "k");
        }
        _ => panic!("expected api key header"),
    }
    assert!(
        matches!(auth_method_for_provider("openai", "k"), AuthMethod::BearerToken(v) if v == "k")
    );

    assert_eq!(default_base_url("anthropic"), "https://api.anthropic.com");
    assert_eq!(
        default_base_url("google"),
        "https://generativelanguage.googleapis.com"
    );
    assert_eq!(default_base_url("other"), "https://api.openai.com");
}

#[test]
fn parse_model_id_valid_and_invalid() {
    let (provider, model) = parse_model_id("openai/gpt-4o").expect("valid model id");
    assert_eq!(provider, "openai");
    assert_eq!(model, "gpt-4o");
    assert!(parse_model_id("invalid").is_err());
}

#[test]
fn text_helpers_cover_limits_and_empty() {
    assert_eq!(log_snippet(" hello ", 10), "hello");
    assert_eq!(log_snippet("", 10), "<empty>");
    assert_eq!(log_snippet("abcd", 4), "abcd");
    assert_eq!(log_snippet("abcdef", 4), "abcd…");
}

#[test]
fn command_formatters() {
    assert_eq!(format_command_details(&[]), None);
    let specs = vec![CommandSpec {
        name: "lint".into(),
        command: "cargo clippy".into(),
        timeout_secs: None,
    }];
    assert_eq!(
        format_command_details(&specs),
        Some("- **lint**: `cargo clippy`".to_string())
    );
}

#[tokio::test]
async fn recent_feedback_filters_orders_and_limits() {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let repo = TaskRepository::new(db.clone(), crate::test_helpers::test_events());

    repo.log_activity(
        Some(&task.id),
        "w1",
        "worker",
        "comment",
        r#"{"body":"ignore worker"}"#,
    )
    .await
    .unwrap();
    repo.log_activity(
        Some(&task.id),
        "pm1",
        "pm",
        "comment",
        r#"{"body":"pm note"}"#,
    )
    .await
    .unwrap();
    repo.log_activity(
        Some(&task.id),
        "r1",
        "task_reviewer",
        "comment",
        r#"{"body":"review note"}"#,
    )
    .await
    .unwrap();
    repo.log_activity(
        Some(&task.id),
        "v1",
        "verification",
        "comment",
        r#"{"body":"verify note"}"#,
    )
    .await
    .unwrap();

    let activity = repo.list_activity(&task.id).await.unwrap();

    let all_feedback = recent_feedback(&activity, 10);
    assert_eq!(all_feedback.len(), 3);
    assert!(all_feedback[0].contains("Lead guidance"));
    assert!(all_feedback[0].contains("pm note"));
    assert!(all_feedback[1].contains("Reviewer feedback"));
    assert!(all_feedback[1].contains("review note"));
    assert!(all_feedback[2].contains("Verification failure"));
    assert!(all_feedback[2].contains("verify note"));

    let capped_feedback = recent_feedback(&activity, 2);
    assert_eq!(capped_feedback.len(), 2);
    assert!(capped_feedback[0].contains("Reviewer feedback"));
    assert!(capped_feedback[1].contains("Verification failure"));
}

#[tokio::test]
async fn initial_user_message_default_and_feedback() {
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let repo = TaskRepository::new(db.clone(), crate::test_helpers::test_events());

    let default_msg = initial_user_message_for_task(&task.id, &state).await;
    assert_eq!(
        default_msg,
        "Start by understanding the task context and execute it fully before stopping."
    );

    repo.log_activity(
        Some(&task.id),
        "pm1",
        "pm",
        "comment",
        r#"{"body":"please fix tests"}"#,
    )
    .await
    .unwrap();
    let feedback_msg = initial_user_message_for_task(&task.id, &state).await;
    assert!(feedback_msg.contains("important feedback"));
    assert!(feedback_msg.contains("please fix tests"));
}

fn activity_entry(
    actor_role: &str,
    event_type: &str,
    body: &str,
    created_at: &str,
) -> djinn_core::models::ActivityEntry {
    djinn_core::models::ActivityEntry {
        id: format!("evt-{created_at}"),
        task_id: Some("t".to_string()),
        actor_id: "sys".to_string(),
        actor_role: actor_role.to_string(),
        event_type: event_type.to_string(),
        payload: serde_json::json!({ "body": body }).to_string(),
        created_at: created_at.to_string(),
    }
}

#[test]
fn latest_ci_feedback_respects_cycle_floor() {
    // Activity is chronological (oldest first), matching `list_activity`.
    let activity = vec![
        // Stale CI comment from an earlier head SHA.
        activity_entry(
            "verification",
            "comment",
            "OLD CI failure",
            "2026-06-01T10:00:00Z",
        ),
        // Reviewer feedback marks the start of the current cycle.
        activity_entry(
            "system",
            "pr_review_feedback",
            "ignored",
            "2026-06-01T11:00:00Z",
        ),
        // Fresh CI comment from the current cycle.
        activity_entry(
            "verification",
            "comment",
            "NEW CI failure",
            "2026-06-01T11:00:01Z",
        ),
    ];

    // With a cycle floor at the reviewer-feedback timestamp, only the fresh CI
    // comment is surfaced — the stale one is skipped.
    let in_cycle = raw_ci_feedback_in_cycle(&activity, Some("2026-06-01T11:00:00Z"))
        .expect("expected in-cycle CI feedback");
    assert!(in_cycle.contains("NEW CI failure"));
    assert!(!in_cycle.contains("OLD CI failure"));

    // A floor newer than every CI comment yields nothing (no stale surfacing).
    assert!(raw_ci_feedback_in_cycle(&activity, Some("2026-06-01T12:00:00Z")).is_none());

    // No floor → most recent CI comment.
    let unbounded = raw_ci_feedback_in_cycle(&activity, None).unwrap();
    assert!(unbounded.contains("NEW CI failure"));
}

#[tokio::test]
async fn initial_user_message_combines_reviewer_and_ci_feedback() {
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let repo = TaskRepository::new(db.clone(), crate::test_helpers::test_events());

    // PR review feedback (system / pr_review_feedback) with at least one inline
    // comment so `pr_review_feedback_context` returns Some(..).
    let feedback_payload = serde_json::json!({
        "pull_number": 7,
        "pr_url": "https://github.com/o/r/pull/7",
        "round": 1,
        "change_request_reviews": [],
        "inline_comments": [{
            "reviewer": "alice",
            "body": "REVIEWER wants this renamed",
            "path": "src/lib.rs",
            "line": 10,
            "html_url": "https://github.com/o/r/pull/7#c1",
        }],
    })
    .to_string();
    repo.log_activity(
        Some(&task.id),
        "system",
        "system",
        "pr_review_feedback",
        &feedback_payload,
    )
    .await
    .unwrap();

    // CI failure comment logged in the same cycle (verification role).
    repo.log_activity(
        Some(&task.id),
        "pr_poller",
        "verification",
        "comment",
        r#"{"body":"**CI checks failed on PR** clippy job FAILED here"}"#,
    )
    .await
    .unwrap();

    let msg = initial_user_message_for_task(&task.id, &state).await;
    // Both sources present in ONE directive.
    assert!(msg.contains("Address ALL of the following"));
    assert!(msg.contains("REVIEWER wants this renamed"));
    assert!(msg.contains("CI checks failed on PR"));
    assert!(msg.contains("(A)"));
    assert!(msg.contains("(B)"));
}

#[tokio::test]
async fn initial_user_message_reviewer_only_preserves_behavior() {
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let repo = TaskRepository::new(db.clone(), crate::test_helpers::test_events());

    let feedback_payload = serde_json::json!({
        "pull_number": 7,
        "pr_url": "https://github.com/o/r/pull/7",
        "round": 1,
        "change_request_reviews": [],
        "inline_comments": [{
            "reviewer": "alice",
            "body": "REVIEWER wants this renamed",
            "path": "src/lib.rs",
            "line": 10,
            "html_url": "https://github.com/o/r/pull/7#c1",
        }],
    })
    .to_string();
    repo.log_activity(
        Some(&task.id),
        "system",
        "system",
        "pr_review_feedback",
        &feedback_payload,
    )
    .await
    .unwrap();

    // No CI/verification comment → reviewer-only message (today's behavior).
    let msg = initial_user_message_for_task(&task.id, &state).await;
    assert!(msg.contains("A human reviewer has requested changes"));
    assert!(msg.contains("REVIEWER wants this renamed"));
    assert!(!msg.contains("Address ALL of the following"));
    assert!(!msg.contains("(B)"));
}

// ─── E5: per-section char budgeting for the combined rework brief ─────────────

/// A `smart_truncate` output always carries one of its byte-accounting markers.
fn is_truncated(s: &str) -> bool {
    s.contains("bytes omitted") || s.contains("bytes total")
}

#[test]
fn combined_budget_small_sections_pass_through_untouched() {
    let reviewer = "reviewer: rename foo to bar";
    let ci = "ci: clippy failed on line 10";
    let (rev_out, ci_out) = budget_combined_sections(reviewer, ci);
    // Both well under any budget → returned verbatim, no truncation marker.
    assert_eq!(rev_out, reviewer);
    assert_eq!(ci_out, ci);
    assert!(!is_truncated(&rev_out));
    assert!(!is_truncated(&ci_out));
}

#[test]
fn combined_budget_oversized_reviewer_does_not_starve_ci() {
    // Huge reviewer blob, modest CI section.
    let reviewer = "R".repeat(COMBINED_BRIEF_TOTAL_CHARS * 3);
    let ci = "CI-DETAIL: the clippy job failed here\n".repeat(20);
    let (rev_out, ci_out) = budget_combined_sections(&reviewer, &ci);

    // Reviewer is clipped to a budget; it cannot consume the whole total.
    assert!(is_truncated(&rev_out));
    assert!(rev_out.len() <= COMBINED_BRIEF_TOTAL_CHARS);
    // CI still keeps at least its floor's worth of room and survives intact.
    assert!(ci_out.contains("CI-DETAIL"));
    assert!(!is_truncated(&ci_out));
    assert!(ci_out.len() <= COMBINED_BRIEF_TOTAL_CHARS);
    // The oversized reviewer is held at/under (total - what CI used).
    assert!(rev_out.len() <= COMBINED_BRIEF_TOTAL_CHARS - COMBINED_BRIEF_SECTION_FLOOR_CHARS + 200);
}

#[test]
fn combined_budget_oversized_ci_does_not_starve_reviewer() {
    let reviewer = "REVIEWER-DETAIL: rename this symbol\n".repeat(20);
    let ci = "C".repeat(COMBINED_BRIEF_TOTAL_CHARS * 3);
    let (rev_out, ci_out) = budget_combined_sections(&reviewer, &ci);

    assert!(is_truncated(&ci_out));
    assert!(ci_out.len() <= COMBINED_BRIEF_TOTAL_CHARS);
    // Reviewer is not starved — it appears in full, untruncated.
    assert!(rev_out.contains("REVIEWER-DETAIL"));
    assert!(!is_truncated(&rev_out));
}

#[test]
fn combined_budget_lends_unused_room_when_both_large() {
    // Both sections far exceed their floors → shared pool split roughly evenly,
    // and each still ends up materially larger than its bare floor.
    let reviewer = "R".repeat(COMBINED_BRIEF_TOTAL_CHARS);
    let ci = "C".repeat(COMBINED_BRIEF_TOTAL_CHARS);
    let (rev_out, ci_out) = budget_combined_sections(&reviewer, &ci);

    assert!(is_truncated(&rev_out));
    assert!(is_truncated(&ci_out));
    // Each gets more than its guaranteed floor (the shared pool is distributed).
    assert!(rev_out.len() > COMBINED_BRIEF_SECTION_FLOOR_CHARS);
    assert!(ci_out.len() > COMBINED_BRIEF_SECTION_FLOOR_CHARS);
    // Combined payload stays bounded by the total (plus small marker overhead).
    assert!(rev_out.len() + ci_out.len() <= COMBINED_BRIEF_TOTAL_CHARS + 400);
}

#[test]
fn combined_budget_single_section_gets_more_than_floor() {
    // When only one section has content, it should be allowed well past the
    // per-section floor (it borrows the empty peer's whole share).
    let reviewer = "R".repeat(COMBINED_BRIEF_TOTAL_CHARS * 2);
    let (rev_out, ci_out) = budget_combined_sections(&reviewer, "");
    assert!(is_truncated(&rev_out));
    assert!(rev_out.len() > COMBINED_BRIEF_SECTION_FLOOR_CHARS * 2);
    assert!(rev_out.len() <= COMBINED_BRIEF_TOTAL_CHARS);
    assert_eq!(ci_out, "");
}
