use std::collections::BTreeSet;

const MIN_IDENTIFIER_LEN: usize = 3;
const MAX_IDENTIFIER_LEN: usize = 64;
const MAX_IDENTIFIERS: usize = 64;

const STOP_WORDS: &[&str] = &[
    "a",
    "an",
    "and",
    "are",
    "but",
    "for",
    "from",
    "into",
    "not",
    "only",
    "the",
    "then",
    "this",
    "that",
    "their",
    "with",
    "without",
    "task",
    "phase",
    "session",
    "repo",
    "map",
    "server",
    "src",
    "design",
    "title",
    "description",
    "memory",
    "refs",
    "text",
    "strings",
    "attached",
    "already",
    "before",
    "after",
    "keep",
    "keeps",
    "using",
    "used",
    "select",
    "selection",
    "render",
    "renderer",
    "cached",
    "cache",
    "contract",
    "contracts",
    "downstream",
    "work",
    "entry",
    "entries",
    "ranked",
    "ranking",
    "boost",
    "boosted",
    "simple",
    "deterministic",
    "fuzzy",
    "search",
    "tests",
    "test",
    "cover",
    "covers",
    "show",
    "shows",
    "ahead",
    "similar",
    "otherwise",
    "baseline",
    "chat",
    "prompt",
    "assembly",
    "wire",
    "wiring",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoMapPersonalizationInput<'a> {
    pub title: Option<&'a str>,
    pub description: Option<&'a str>,
    pub design: Option<&'a str>,
    pub memory_refs: &'a [String],
}

pub fn extract_identifier_candidates(input: &RepoMapPersonalizationInput<'_>) -> Vec<String> {
    let mut seen = BTreeSet::new();

    for text in [input.title, input.description, input.design]
        .into_iter()
        .flatten()
    {
        collect_identifiers(text, &mut seen);
    }

    for memory_ref in input.memory_refs {
        collect_identifiers(memory_ref, &mut seen);
    }

    seen.into_iter().take(MAX_IDENTIFIERS).collect()
}

fn collect_identifiers(text: &str, output: &mut BTreeSet<String>) {
    let normalized = normalize_text(text);
    for token in normalized.split_whitespace() {
        if is_candidate_token(token) {
            output.insert(token.to_string());
        }
    }
}

fn normalize_text(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch.to_ascii_lowercase());
        } else {
            normalized.push(' ');
        }
    }
    normalized
}

fn is_candidate_token(token: &str) -> bool {
    if token.len() < MIN_IDENTIFIER_LEN || token.len() > MAX_IDENTIFIER_LEN {
        return false;
    }
    if STOP_WORDS.contains(&token) {
        return false;
    }
    if token.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    let has_alpha = token.chars().any(|ch| ch.is_ascii_alphabetic());
    let has_digit = token.chars().any(|ch| ch.is_ascii_digit());
    has_digit || has_alpha && !is_low_signal_alpha_word(token)
}

fn is_low_signal_alpha_word(token: &str) -> bool {
    token.len() <= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_normalized_identifiers_from_all_inputs() {
        let memory_refs = vec![
            "decisions/adr-043-repository-map-scip-powered-structural-context-for-agent-sessions"
                .to_string(),
            "notes/RepoMapQueryHelper".to_string(),
        ];
        let input = RepoMapPersonalizationInput {
            title: Some("Phase 2: Extract task-aware identifiers for RepoMapQueryHelper"),
            description: Some(
                "Parse session title/description/design text and prefer repo_map.rs plus TaskSession42.",
            ),
            design: Some(
                "Bias selection toward repo_map_personalization.rs and relationship display text like symbol-ref repo_map.rs",
            ),
            memory_refs: &memory_refs,
        };

        let identifiers = extract_identifier_candidates(&input);

        assert!(!identifiers.contains(&"repo".to_string()));
        assert!(!identifiers.contains(&"map".to_string()));
        assert!(!identifiers.contains(&"task".to_string()));
        assert!(identifiers.contains(&"repomapqueryhelper".to_string()));
        assert!(!identifiers.contains(&"repo_map".to_string()));
        assert!(identifiers.contains(&"tasksession42".to_string()));
        assert!(!identifiers.contains(&"repo_map_personalization".to_string()));
        assert!(identifiers.contains(&"repository".to_string()));
        assert!(identifiers.contains(&"scip".to_string()));
        assert!(identifiers.contains(&"agent".to_string()));
        assert!(identifiers.contains(&"sessions".to_string()));
        assert!(identifiers.contains(&"relationship".to_string()));
        assert!(identifiers.contains(&"symbol".to_string()));
        assert!(!identifiers.contains(&"ref".to_string()));
        assert!(!identifiers.contains(&"043".to_string()));
    }
}
