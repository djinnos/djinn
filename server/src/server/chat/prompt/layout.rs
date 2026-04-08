#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server::chat) enum PromptSegmentStability {
    Stable,
    Dynamic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server::chat) struct PromptSegment {
    pub(in crate::server::chat) text: String,
    pub(in crate::server::chat) stability: PromptSegmentStability,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::server::chat) struct SystemPromptLayout {
    pub(in crate::server::chat) stable_prefix: Vec<PromptSegment>,
    pub(in crate::server::chat) dynamic_tail: Option<String>,
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

pub(in crate::server::chat) fn compose_system_prompt_segments(
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

pub(in crate::server::chat) fn partition_system_prompt_segments(
    segments: &[PromptSegment],
) -> SystemPromptLayout {
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

#[cfg(test)]
pub(in crate::server::chat) fn compose_system_prompt(
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
