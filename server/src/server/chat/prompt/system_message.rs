use djinn_provider::message::{CacheBreakpoint, ContentBlock, Message, MessageMeta, Role};

use super::layout::{compose_system_prompt_segments, partition_system_prompt_segments};

pub(in crate::server::chat) const ANTHROPIC_CACHE_BREAKPOINT_KEY: &str =
    "anthropic_cache_breakpoint";
pub(in crate::server::chat) const ANTHROPIC_STABLE_PREFIX_KIND: &str = "stable_prefix";

pub(in crate::server::chat) fn system_message_metadata(
    model: &str,
    has_stable_prefix: bool,
) -> Option<MessageMeta> {
    if model.starts_with("anthropic/") && has_stable_prefix {
        Some(MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(serde_json::json!({
                ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                    kind: Some(ANTHROPIC_STABLE_PREFIX_KIND.to_string()),
                }
            })),
        })
    } else {
        None
    }
}

pub(in crate::server::chat) fn build_system_message(
    base_prompt: &str,
    project_context: Option<&str>,
    client_system: Option<&str>,
    model: &str,
) -> Message {
    let segments =
        compose_system_prompt_segments(base_prompt, project_context, client_system);
    let layout = partition_system_prompt_segments(&segments);
    let metadata = system_message_metadata(model, !layout.stable_prefix.is_empty());

    let mut content: Vec<ContentBlock> = layout
        .stable_prefix
        .into_iter()
        .map(|segment| ContentBlock::text(segment.text))
        .collect();
    if let Some(dynamic_tail) = layout.dynamic_tail {
        content.push(ContentBlock::text(dynamic_tail));
    }

    Message {
        role: Role::System,
        content,
        metadata,
    }
}
