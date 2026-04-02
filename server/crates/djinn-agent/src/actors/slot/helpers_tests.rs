use super::helpers::*;
use super::{MERGE_CONFLICT_PREFIX, MERGE_VALIDATION_PREFIX};
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
fn parse_merge_validation_metadata_patterns() {
    let raw = r#"{"base_branch":"feature","merge_target":"main","command":"merge --no-ff","cwd":"/tmp/repo","exit_code":1,"stdout":"out","stderr":"err"}"#;
    assert!(parse_merge_validation_metadata(&format!("{MERGE_VALIDATION_PREFIX}{raw}")).is_some());
    assert!(parse_merge_validation_metadata(raw).is_none());
    assert!(parse_merge_validation_metadata(&format!("{MERGE_VALIDATION_PREFIX}{{oops")).is_none());
}

#[test]
fn provider_helpers_cover_branches() {
    use crate::provider::{AuthMethod, FormatFamily};

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

    let anthropic = capabilities_for_provider("anthropic");
    assert!(anthropic.streaming);
    assert_eq!(anthropic.max_tokens_default, Some(64_000));

    let synthetic = capabilities_for_provider("synthetic-provider");
    assert!(!synthetic.streaming);
    assert_eq!(synthetic.max_tokens_default, None);

    let local = capabilities_for_provider("local-provider");
    assert!(!local.streaming);

    let default_caps = capabilities_for_provider("openai");
    let expected_default = crate::provider::ProviderCapabilities::default();
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
async fn resume_context_for_rejection_conflict_and_no_activity() {
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let repo = TaskRepository::new(db.clone(), crate::test_helpers::test_events());

    repo.log_activity(
        Some(&task.id),
        "reviewer",
        "task_reviewer",
        "status_changed",
        r#"{"from_status":"in_task_review","to_status":"open","reason":"Needs tests"}"#,
    )
    .await
    .unwrap();
    let msg = resume_context_for_task(&task.id, &state).await;
    assert!(msg.contains("IMPORTANT"));
    assert!(msg.contains("Needs tests"));

    let task2 = create_test_task(&db, &project.id, &epic.id).await;
    repo.log_activity(
        Some(&task2.id),
        "system",
        "system",
        "merge_conflict",
        r#"{"base_branch":"feature","merge_target":"main","conflicting_files":["src/lib.rs"]}"#,
    )
    .await
    .unwrap();
    let msg2 = resume_context_for_task(&task2.id, &state).await;
    assert!(msg2.contains("merge conflict"));
    assert!(msg2.contains("src/lib.rs"));

    let task3 = create_test_task(&db, &project.id, &epic.id).await;
    let msg3 = resume_context_for_task(&task3.id, &state).await;
    assert!(msg3.contains("previous submission was rejected"));
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
