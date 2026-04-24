use crate::server::chat::prompt::layout::{
    PromptSegmentStability, compose_system_prompt, compose_system_prompt_segments,
    partition_system_prompt_segments,
};
use crate::server::chat::prompt::system_message::{
    ANTHROPIC_CACHE_BREAKPOINT_KEY, ANTHROPIC_STABLE_PREFIX_KIND, build_system_message,
    system_message_metadata,
};
use serde_json::json;

use super::super::DJINN_CHAT_SYSTEM_PROMPT;

#[test]
fn system_prompt_contains_base_prompt_first_and_project_block_before_client_system() {
    let project_context = "## Current Project\n**Name**: Demo  **Path**: /tmp/demo\n**Open epics**: 1  **Open tasks**: 2\n**Brief**: hello";
    let client_system = "client system message";
    let prompt =
        compose_system_prompt(DJINN_CHAT_SYSTEM_PROMPT, Some(project_context), Some(client_system));

    let base = DJINN_CHAT_SYSTEM_PROMPT.trim();
    assert!(prompt.starts_with(base));
    let base_pos = prompt.find(base).unwrap();
    let project_pos = prompt.find("## Current Project").unwrap();
    let client_pos = prompt.find(client_system).unwrap();
    assert!(base_pos <= project_pos);
    assert!(project_pos < client_pos);
}

#[test]
fn system_prompt_segments_mark_stable_project_context_for_caching() {
    let project_context = "## Current Project\nproject";
    let client_system = "be concise";

    let segments = compose_system_prompt_segments(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some(project_context),
        Some(client_system),
    );

    assert_eq!(segments.len(), 3);
    assert_eq!(segments[0].stability, PromptSegmentStability::Stable);
    assert_eq!(segments[1].text, project_context);
    assert_eq!(segments[1].stability, PromptSegmentStability::Stable);
    assert_eq!(segments[2].text, client_system);
    assert_eq!(segments[2].stability, PromptSegmentStability::Dynamic);
}

#[test]
fn system_message_metadata_uses_explicit_anthropic_breakpoint_contract() {
    let metadata = system_message_metadata("anthropic/claude-3-5-sonnet", true)
        .expect("anthropic stable prefix should emit metadata");
    let provider_data = metadata.provider_data.expect("provider data");

    assert_eq!(
        provider_data,
        json!({
            ANTHROPIC_CACHE_BREAKPOINT_KEY: {
                "kind": ANTHROPIC_STABLE_PREFIX_KIND,
            }
        })
    );
    assert!(system_message_metadata("openai/gpt-4o", true).is_none());
    assert!(system_message_metadata("anthropic/claude-3-5-sonnet", false).is_none());
}

#[test]
fn build_system_message_preserves_segment_ordering() {
    let project_context = "## Current Project\nproject";
    let message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some(project_context),
        Some("volatile client system"),
        "anthropic/claude-3-5-sonnet",
    );

    assert_eq!(message.content.len(), 3);
    assert_eq!(
        message.content[0].as_text(),
        Some(DJINN_CHAT_SYSTEM_PROMPT.trim())
    );
    assert_eq!(message.content[1].as_text(), Some(project_context));
    assert_eq!(message.content[2].as_text(), Some("volatile client system"));
}

#[test]
fn build_system_message_skips_cache_breakpoint_for_non_anthropic() {
    let openai_message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some("## Current Project\nproject"),
        None,
        "openai/gpt-4o",
    );
    assert!(openai_message.metadata.is_none());

    let anthropic_base_only = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        None,
        Some("volatile client system"),
        "anthropic/claude-3-5-sonnet",
    );
    assert!(anthropic_base_only.metadata.is_some());
}

#[test]
fn compose_segments_skips_empty_optional_segments() {
    let segments = compose_system_prompt_segments(DJINN_CHAT_SYSTEM_PROMPT, Some(""), Some("  \n "));
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].text, DJINN_CHAT_SYSTEM_PROMPT.trim());
}

#[test]
fn partition_system_prompt_segments_extracts_explicit_dynamic_tail_boundary() {
    let segments = compose_system_prompt_segments(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some("project ctx"),
        Some("client system\n\ntask context"),
    );

    let layout = partition_system_prompt_segments(&segments);

    assert_eq!(layout.stable_prefix.len(), 2);
    assert_eq!(
        layout.stable_prefix[0].text,
        DJINN_CHAT_SYSTEM_PROMPT.trim()
    );
    assert_eq!(layout.stable_prefix[1].text, "project ctx");
    assert_eq!(
        layout.dynamic_tail.as_deref(),
        Some("client system\n\ntask context")
    );
}

#[test]
fn build_system_message_only_dynamic_tail_never_creates_cacheable_trailing_block() {
    let message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        None,
        Some("client system\n\ntask context"),
        "anthropic/claude-3-5-sonnet",
    );

    assert_eq!(message.content.len(), 2);
    assert_eq!(
        message.content[0].as_text(),
        Some(DJINN_CHAT_SYSTEM_PROMPT.trim())
    );
    assert_eq!(
        message.content[1].as_text(),
        Some("client system\n\ntask context")
    );
    assert!(message.metadata.is_some());
}
