use std::collections::BTreeSet;

use djinn_core::models::{NoteSearchResult, Task};

const MIN_IDENTIFIER_LEN: usize = 3;
const MAX_IDENTIFIER_LEN: usize = 64;
const MAX_IDENTIFIERS: usize = 64;
const MAX_NOTE_HINTS: usize = 8;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoMapNoteSearchInput {
    pub original_ref: String,
    pub permalink: String,
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RepoMapNoteHint {
    pub permalink: String,
    pub title: String,
    pub snippet: String,
    pub normalized_tokens: Vec<String>,
}

pub trait RepoMapNoteSearcher {
    type Error;

    fn search<'a>(
        &'a self,
        project_id: &'a str,
        query: &'a str,
        task_id: Option<&'a str>,
        limit: usize,
    ) -> impl std::future::Future<Output = Result<Vec<NoteSearchResult>, Self::Error>> + Send + 'a;
}

impl<'a> RepoMapPersonalizationInput<'a> {
    pub fn from_task(task: &'a Task, memory_refs: &'a [String]) -> Self {
        Self {
            title: Some(task.title.as_str()),
            description: Some(task.description.as_str()),
            design: Some(task.design.as_str()),
            memory_refs,
        }
    }
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

pub fn parse_task_memory_refs(memory_refs: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(memory_refs).unwrap_or_else(|_| Vec::new())
}

pub fn note_search_inputs_from_memory_refs(memory_refs: &str) -> Vec<RepoMapNoteSearchInput> {
    let mut seen = BTreeSet::new();

    parse_task_memory_refs(memory_refs)
        .into_iter()
        .filter_map(|memory_ref| {
            let permalink = memory_ref.trim();
            if permalink.is_empty() {
                return None;
            }

            let query = normalize_memory_ref_query(permalink);
            if query.is_empty() || !seen.insert((permalink.to_string(), query.clone())) {
                return None;
            }

            Some(RepoMapNoteSearchInput {
                original_ref: memory_ref.clone(),
                permalink: permalink.to_string(),
                query,
            })
        })
        .collect()
}

pub async fn derive_note_hints_from_task_memory_refs<S>(
    searcher: &S,
    task: &Task,
) -> Result<Vec<RepoMapNoteHint>, S::Error>
where
    S: RepoMapNoteSearcher + Sync,
{
    let search_inputs = note_search_inputs_from_memory_refs(&task.memory_refs);
    if search_inputs.is_empty() {
        return Ok(Vec::new());
    }

    let mut seen = BTreeSet::new();
    let mut hints = Vec::new();

    for input in search_inputs {
        let results = searcher
            .search(
                &task.project_id,
                &input.query,
                Some(&task.id),
                MAX_NOTE_HINTS,
            )
            .await?;

        for result in results {
            if result.permalink != input.permalink {
                continue;
            }

            let hint = note_search_result_to_hint(result);
            if seen.insert(hint.permalink.clone()) {
                hints.push(hint);
            }
        }
    }

    hints.sort();
    Ok(hints)
}

fn note_search_result_to_hint(result: NoteSearchResult) -> RepoMapNoteHint {
    let mut tokens = BTreeSet::new();
    collect_identifiers(&result.title, &mut tokens);
    collect_identifiers(&result.snippet, &mut tokens);
    collect_identifiers(&result.permalink, &mut tokens);

    RepoMapNoteHint {
        permalink: result.permalink,
        title: result.title,
        snippet: result.snippet,
        normalized_tokens: tokens.into_iter().collect(),
    }
}

fn normalize_memory_ref_query(memory_ref: &str) -> String {
    normalize_text(memory_ref)
        .split_whitespace()
        .filter(|token| !token.chars().all(|ch| ch.is_ascii_digit()))
        .collect::<Vec<_>>()
        .join(" ")
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
    use djinn_core::models::Task;

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

    #[test]
    fn parses_memory_refs_into_deterministic_note_queries() {
        let inputs = note_search_inputs_from_memory_refs(
            r#"["decisions/adr-043-repository-map-scip-powered-structural-context-for-agent-sessions","notes/RepoMapQueryHelper"]"#,
        );

        assert_eq!(
            inputs,
            vec![
                RepoMapNoteSearchInput {
                    original_ref: "decisions/adr-043-repository-map-scip-powered-structural-context-for-agent-sessions"
                        .to_string(),
                    permalink: "decisions/adr-043-repository-map-scip-powered-structural-context-for-agent-sessions"
                        .to_string(),
                    query: "decisions adr repository map scip powered structural context for agent sessions"
                        .to_string(),
                },
                RepoMapNoteSearchInput {
                    original_ref: "notes/RepoMapQueryHelper".to_string(),
                    permalink: "notes/RepoMapQueryHelper".to_string(),
                    query: "notes repomapqueryhelper".to_string(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn derive_note_hints_returns_empty_when_task_has_no_memory_refs() {
        let task = test_task("[]");
        let searcher = FakeNoteSearcher::default();

        let hints = derive_note_hints_from_task_memory_refs(&searcher, &task)
            .await
            .expect("derive succeeds");

        assert!(hints.is_empty());
    }

    #[tokio::test]
    async fn derive_note_hints_returns_empty_when_search_has_no_matching_notes() {
        let task = test_task(r#"["decisions/adr-043-repository-map"]"#);
        let searcher = FakeNoteSearcher {
            results: vec![NoteSearchResult {
                id: "note-2".to_string(),
                permalink: "decisions/other-note".to_string(),
                title: "Other note".to_string(),
                folder: "decisions".to_string(),
                note_type: "adr".to_string(),
                snippet: "something else".to_string(),
                score: 0.5,
            }],
        };

        let hints = derive_note_hints_from_task_memory_refs(&searcher, &task)
            .await
            .expect("derive succeeds");

        assert!(hints.is_empty());
    }

    #[tokio::test]
    async fn derive_note_hints_is_deterministic_for_stable_search_results() {
        let task = test_task(r#"["decisions/adr-043-repository-map"]"#);
        let result = NoteSearchResult {
            id: "note-1".to_string(),
            permalink: "decisions/adr-043-repository-map".to_string(),
            title: "ADR 043 Repository Map".to_string(),
            folder: "decisions".to_string(),
            note_type: "adr".to_string(),
            snippet: "SCIP-powered structural context for agent sessions".to_string(),
            score: 1.0,
        };
        let searcher = FakeNoteSearcher {
            results: vec![result.clone(), result],
        };

        let hints = derive_note_hints_from_task_memory_refs(&searcher, &task)
            .await
            .expect("derive succeeds");

        assert_eq!(
            hints,
            vec![RepoMapNoteHint {
                permalink: "decisions/adr-043-repository-map".to_string(),
                title: "ADR 043 Repository Map".to_string(),
                snippet: "SCIP-powered structural context for agent sessions".to_string(),
                normalized_tokens: vec![
                    "agent".to_string(),
                    "context".to_string(),
                    "decisions".to_string(),
                    "powered".to_string(),
                    "repository".to_string(),
                    "scip".to_string(),
                    "sessions".to_string(),
                    "structural".to_string(),
                ],
            }]
        );
    }

    #[derive(Default)]
    struct FakeNoteSearcher {
        results: Vec<NoteSearchResult>,
    }

    impl RepoMapNoteSearcher for FakeNoteSearcher {
        type Error = ();

        async fn search<'a>(
            &'a self,
            _project_id: &'a str,
            _query: &'a str,
            _task_id: Option<&'a str>,
            _limit: usize,
        ) -> Result<Vec<NoteSearchResult>, Self::Error> {
            Ok(self.results.clone())
        }
    }

    fn test_task(memory_refs: &str) -> Task {
        Task {
            id: "task-1".to_string(),
            project_id: "project-1".to_string(),
            short_id: "t1".to_string(),
            epic_id: None,
            title: "Repo map task".to_string(),
            description: "derive note hints".to_string(),
            design: "use note search".to_string(),
            issue_type: "task".to_string(),
            status: "open".to_string(),
            priority: 1,
            owner: "system".to_string(),
            labels: "[]".to_string(),
            acceptance_criteria: "[]".to_string(),
            reopen_count: 0,
            continuation_count: 0,
            verification_failure_count: 0,
            total_reopen_count: 0,
            total_verification_failure_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "".to_string(),
            updated_at: "".to_string(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: memory_refs.to_string(),
            agent_type: None,
            unresolved_blocker_count: 0,
        }
    }
}
