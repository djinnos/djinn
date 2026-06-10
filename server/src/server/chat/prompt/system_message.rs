use djinn_agent::actors::slot::{format_family_for_provider, parse_model_id};
use djinn_provider::message::{CacheBreakpoint, ContentBlock, Message, MessageMeta, Role};
use djinn_provider::provider::FormatFamily;

use super::layout::{compose_system_prompt_segments, partition_system_prompt_segments};

pub(in crate::server::chat) const ANTHROPIC_CACHE_BREAKPOINT_KEY: &str =
    "anthropic_cache_breakpoint";
pub(in crate::server::chat) const ANTHROPIC_STABLE_PREFIX_KIND: &str = "stable_prefix";

/// Whether the model is served over the Anthropic wire format and therefore
/// consumes `cache_control` breakpoint metadata. Routed through
/// [`format_family_for_provider`] so Anthropic-compatible vendors (MiniMax
/// coding plan, …) get the same stable-prefix caching as native Anthropic.
fn speaks_anthropic_format(model: &str) -> bool {
    parse_model_id(model).is_ok_and(|(provider_id, model_name)| {
        format_family_for_provider(&provider_id, &model_name) == FormatFamily::Anthropic
    })
}

pub(in crate::server::chat) fn system_message_metadata(
    model: &str,
    has_stable_prefix: bool,
) -> Option<MessageMeta> {
    if speaks_anthropic_format(model) && has_stable_prefix {
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
    let segments = compose_system_prompt_segments(base_prompt, project_context, client_system);
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
