use crate::server::chat::context::{REPO_MAP_SYSTEM_HEADER, format_repo_map_block};
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
fn system_prompt_contains_base_prompt_first_and_project_block_before_repo_map_and_client_system() {
    let project_context = "## Current Project\n**Name**: Demo  **Path**: /tmp/demo\n**Open epics**: 1  **Open tasks**: 2\n**Brief**: hello";
    let repo_map = format_repo_map_block("src/main.rs\n  fn main()", None);
    let client_system = "client system message";
    let prompt = compose_system_prompt(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some(project_context),
        Some(&repo_map),
        Some(client_system),
    );

    let base = DJINN_CHAT_SYSTEM_PROMPT.trim();
    assert!(prompt.starts_with(base));
    let base_pos = prompt.find(base).unwrap();
    let project_pos = prompt.find("## Current Project").unwrap();
    let repo_map_pos = prompt.find(REPO_MAP_SYSTEM_HEADER).unwrap();
    let client_pos = prompt.find(client_system).unwrap();
    assert!(base_pos <= project_pos);
    assert!(project_pos < repo_map_pos);
    assert!(repo_map_pos < client_pos);
}

#[test]
fn system_prompt_segments_mark_stable_project_and_repo_map_context_for_caching() {
    let project_context = "## Current Project\nproject";
    let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);
    let client_system = "be concise";

    let segments = compose_system_prompt_segments(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some(project_context),
        Some(&repo_map),
        Some(client_system),
    );

    assert_eq!(segments.len(), 4);
    assert_eq!(segments[0].stability, PromptSegmentStability::Stable);
    assert_eq!(segments[1].text, project_context);
    assert_eq!(segments[1].stability, PromptSegmentStability::Stable);
    assert_eq!(segments[2].text, repo_map);
    assert_eq!(segments[2].stability, PromptSegmentStability::Stable);
    assert_eq!(segments[3].text, client_system);
    assert_eq!(segments[3].stability, PromptSegmentStability::Dynamic);
}

#[test]
fn compose_segments_document_chat_owned_stable_taxonomy() {
    let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);
    let segments = compose_system_prompt_segments(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some("## Current Project\nproject"),
        Some(&repo_map),
        Some("volatile client system"),
    );

    let segment_texts: Vec<_> = segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect();
    assert_eq!(
        segment_texts,
        vec![
            DJINN_CHAT_SYSTEM_PROMPT.trim(),
            "## Current Project\nproject",
            repo_map.as_str(),
            "volatile client system",
        ],
        "chat.rs owns base prompt, project context, repo map, and dynamic tail; tool definitions are inserted later by provider request assembly"
    );
    assert_eq!(segments[0].stability, PromptSegmentStability::Stable);
    assert_eq!(segments[1].stability, PromptSegmentStability::Stable);
    assert_eq!(segments[2].stability, PromptSegmentStability::Stable);
    assert_eq!(segments[3].stability, PromptSegmentStability::Dynamic);
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
fn build_system_message_preserves_repo_map_as_its_own_content_block() {
    let project_context = "## Current Project\nproject";
    let repo_map = format_repo_map_block(
        "src/lib.rs\n  pub fn run",
        Some("reference/repo-maps/repository-map-deadbeef"),
    );
    let message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some(project_context),
        Some(&repo_map),
        Some("volatile client system"),
        "anthropic/claude-3-5-sonnet",
    );

    assert_eq!(message.content.len(), 4);
    assert_eq!(
        message.content[0].as_text(),
        Some(DJINN_CHAT_SYSTEM_PROMPT.trim())
    );
    assert_eq!(message.content[1].as_text(), Some(project_context));
    assert_eq!(message.content[2].as_text(), Some(repo_map.as_str()));
    assert!(
        message.content[2]
            .as_text()
            .expect("repo map block text")
            .contains("Source note: memory://reference/repo-maps/repository-map-deadbeef")
    );
    assert_eq!(message.content[3].as_text(), Some("volatile client system"));
}

#[test]
fn build_system_message_keeps_dynamic_context_in_a_trailing_block() {
    let project_context = "## Current Project\nproject";
    let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);
    let client_system = "client-specific instruction";
    let task_context = "task-specific instruction";
    let combined_dynamic = format!("{client_system}\n\n{task_context}");

    let message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some(project_context),
        Some(&repo_map),
        Some(&combined_dynamic),
        "anthropic/claude-3-5-sonnet",
    );

    assert_eq!(message.content.len(), 4);
    assert_eq!(
        message.content[0].as_text(),
        Some(DJINN_CHAT_SYSTEM_PROMPT.trim())
    );
    assert_eq!(message.content[1].as_text(), Some(project_context));
    assert_eq!(message.content[2].as_text(), Some(repo_map.as_str()));
    assert_eq!(
        message.content[3].as_text(),
        Some(combined_dynamic.as_str())
    );
}

#[test]
fn build_system_message_keeps_stable_order_when_optional_contexts_are_missing() {
    let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);
    let with_repo_map_only = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        None,
        Some(&repo_map),
        Some("volatile client system"),
        "anthropic/claude-3-5-sonnet",
    );
    assert_eq!(with_repo_map_only.content.len(), 3);
    assert_eq!(
        with_repo_map_only.content[0].as_text(),
        Some(DJINN_CHAT_SYSTEM_PROMPT.trim())
    );
    assert_eq!(
        with_repo_map_only.content[1].as_text(),
        Some(repo_map.as_str())
    );
    assert_eq!(
        with_repo_map_only.content[2].as_text(),
        Some("volatile client system")
    );

    let with_project_only = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some("## Current Project\nproject"),
        None,
        Some("volatile client system"),
        "anthropic/claude-3-5-sonnet",
    );
    assert_eq!(with_project_only.content.len(), 3);
    assert_eq!(
        with_project_only.content[0].as_text(),
        Some(DJINN_CHAT_SYSTEM_PROMPT.trim())
    );
    assert_eq!(
        with_project_only.content[1].as_text(),
        Some("## Current Project\nproject")
    );
    assert_eq!(
        with_project_only.content[2].as_text(),
        Some("volatile client system")
    );
}

#[test]
fn build_system_message_skips_cache_breakpoint_for_non_anthropic_or_without_repo_map() {
    let openai_message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some("## Current Project\nproject"),
        Some(&format_repo_map_block("src/lib.rs", None)),
        None,
        "openai/gpt-4o",
    );
    assert!(openai_message.metadata.is_none());

    let anthropic_without_project_or_repo = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        None,
        None,
        Some("volatile client system"),
        "anthropic/claude-3-5-sonnet",
    );
    assert!(anthropic_without_project_or_repo.metadata.is_some());
    assert!(
        anthropic_without_project_or_repo
            .text_content()
            .contains("volatile client system")
    );
}

#[test]
fn system_prompt_includes_repo_map_block_when_available() {
    let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);
    let prompt = compose_system_prompt(DJINN_CHAT_SYSTEM_PROMPT, None, Some(&repo_map), None);
    assert!(prompt.contains(REPO_MAP_SYSTEM_HEADER));
    assert!(prompt.contains("src/lib.rs"));
}

#[test]
fn compose_segments_skips_empty_optional_segments() {
    let segments =
        compose_system_prompt_segments(DJINN_CHAT_SYSTEM_PROMPT, Some(""), None, Some("  \n "));
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].text, DJINN_CHAT_SYSTEM_PROMPT.trim());
}

#[test]
fn compose_segments_skips_whitespace_only_repo_map() {
    let segments =
        compose_system_prompt_segments(DJINN_CHAT_SYSTEM_PROMPT, None, Some("   "), None);
    assert_eq!(segments.len(), 1);
}

#[test]
fn compose_segments_preserves_order_when_middle_segment_absent() {
    let segments = compose_system_prompt_segments(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some("project ctx"),
        None,
        Some("client system"),
    );
    assert_eq!(segments.len(), 3);
    assert_eq!(segments[0].text, DJINN_CHAT_SYSTEM_PROMPT.trim());
    assert_eq!(segments[1].text, "project ctx");
    assert_eq!(segments[1].stability, PromptSegmentStability::Stable);
    assert_eq!(segments[2].text, "client system");
    assert_eq!(segments[2].stability, PromptSegmentStability::Dynamic);
}

#[test]
fn partition_system_prompt_segments_extracts_explicit_dynamic_tail_boundary() {
    let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);
    let segments = compose_system_prompt_segments(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some("project ctx"),
        Some(&repo_map),
        Some("client system\n\ntask context"),
    );

    let layout = partition_system_prompt_segments(&segments);

    assert_eq!(layout.stable_prefix.len(), 3);
    assert_eq!(
        layout.stable_prefix[0].text,
        DJINN_CHAT_SYSTEM_PROMPT.trim()
    );
    assert_eq!(layout.stable_prefix[1].text, "project ctx");
    assert_eq!(layout.stable_prefix[2].text, repo_map);
    assert_eq!(
        layout.dynamic_tail.as_deref(),
        Some("client system\n\ntask context")
    );
}

#[test]
fn build_system_message_repo_map_remains_stable_prefix_when_project_context_missing() {
    let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);

    let message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        None,
        Some(&repo_map),
        Some("client system\n\ntask context"),
        "anthropic/claude-3-5-sonnet",
    );

    assert_eq!(message.content.len(), 3);
    assert_eq!(
        message.content[0].as_text(),
        Some(DJINN_CHAT_SYSTEM_PROMPT.trim())
    );
    assert_eq!(message.content[1].as_text(), Some(repo_map.as_str()));
    assert_eq!(
        message.content[2].as_text(),
        Some("client system\n\ntask context")
    );
    assert!(message.metadata.is_some());
}

#[test]
fn build_system_message_only_dynamic_tail_never_creates_cacheable_trailing_block() {
    let message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        None,
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

#[test]
fn build_system_message_no_dynamic_tail_when_client_system_blank() {
    let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);
    let message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some("project ctx"),
        Some(&repo_map),
        Some(""),
        "anthropic/claude-3-5-sonnet",
    );
    assert_eq!(message.content.len(), 3);
    assert_eq!(
        message.content[0].as_text(),
        Some(DJINN_CHAT_SYSTEM_PROMPT.trim())
    );
    assert_eq!(message.content[1].as_text(), Some("project ctx"));
    assert_eq!(message.content[2].as_text(), Some(repo_map.as_str()));
}
