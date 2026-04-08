use djinn_agent::message::{CacheBreakpoint, ContentBlock, Message, MessageMeta, Role};

pub(super) const ANTHROPIC_CACHE_BREAKPOINT_KEY: &str = "anthropic_cache_breakpoint";
pub(super) const ANTHROPIC_STABLE_PREFIX_KIND: &str = "stable_prefix";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PromptSegmentStability {
    Stable,
    Dynamic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PromptSegment {
    pub(super) text: String,
    pub(super) stability: PromptSegmentStability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SystemPromptLayout {
    pub(super) stable_prefix: Vec<PromptSegment>,
    pub(super) dynamic_tail: Option<String>,
}

fn prompt_segment(text: impl Into<String>) -> PromptSegment {
    PromptSegment {
        text: text.into(),
        stability: PromptSegmentStability::Dynamic,
    }
}

fn cached_prompt_segment(text: impl Into<String>) -> PromptSegment {
    PromptSegment {
        text: text.into(),
        stability: PromptSegmentStability::Stable,
    }
}

pub(super) fn compose_system_prompt_segments(
    base_prompt: &str,
    project_context: Option<&str>,
    repo_map_context: Option<&str>,
    client_system: Option<&str>,
) -> Vec<PromptSegment> {
    let mut stable_prefix = vec![cached_prompt_segment(base_prompt.trim())];
    if let Some(project_context) = project_context.filter(|s| !s.trim().is_empty()) {
        stable_prefix.push(cached_prompt_segment(project_context));
    }
    if let Some(repo_map_context) = repo_map_context.filter(|s| !s.trim().is_empty()) {
        stable_prefix.push(cached_prompt_segment(repo_map_context));
    }

    let dynamic_tail = client_system
        .filter(|s| !s.trim().is_empty())
        .map(prompt_segment);

    stable_prefix.into_iter().chain(dynamic_tail).collect()
}

pub(super) fn partition_system_prompt_segments(segments: &[PromptSegment]) -> SystemPromptLayout {
    let stable_prefix = segments
        .iter()
        .filter(|segment| segment.stability == PromptSegmentStability::Stable)
        .cloned()
        .collect();
    let dynamic_tail = segments
        .iter()
        .filter(|segment| segment.stability == PromptSegmentStability::Dynamic)
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    SystemPromptLayout {
        stable_prefix,
        dynamic_tail: (!dynamic_tail.trim().is_empty()).then_some(dynamic_tail),
    }
}

pub(super) fn system_message_metadata(model: &str, has_stable_prefix: bool) -> Option<MessageMeta> {
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

#[cfg(test)]
pub(super) fn compose_system_prompt(
    base_prompt: &str,
    project_context: Option<&str>,
    repo_map_context: Option<&str>,
    client_system: Option<&str>,
) -> String {
    compose_system_prompt_segments(
        base_prompt,
        project_context,
        repo_map_context,
        client_system,
    )
    .into_iter()
    .map(|segment| segment.text)
    .collect::<Vec<_>>()
    .join("\n\n")
}

pub(super) fn build_system_message(
    base_prompt: &str,
    project_context: Option<&str>,
    repo_map_context: Option<&str>,
    client_system: Option<&str>,
    model: &str,
) -> Message {
    let segments = compose_system_prompt_segments(
        base_prompt,
        project_context,
        repo_map_context,
        client_system,
    );
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
