use crate::tools::epic_ops::AcceptanceCriterionItem;
use djinn_db::ProposalRepository;
use djinn_spec_lint::SpecLintResultV1;
use serde::{Deserialize, Serialize};

// Deterministic Definition-of-Ready evaluator for proposals.
//
// No LLM involvement — pure heuristic checks over body text, acceptance
// criteria, and target count.  Structured results are easy to turn into
// human-readable error strings by callers such as `proposal_update`,
// `proposal_signoff`, and `proposal_graduate`.

// ── Public result types ────────────────────────────────────────────────────

/// Aggregate readiness result.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProposalReadinessResult {
    pub ready: bool,
    pub failures: Vec<ReadinessFailure>,
}

/// Readiness for the current persisted proposal head, including its repository
/// lint summary. Unlike [`ProposalReadinessResult`], this result is tied to an
/// immutable stored snapshot and therefore must be constructed asynchronously.
///
/// `latest_lint` is absent only when the committed head could not be resolved
/// or linted. Those cases always append a failure and leave `ready` false.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct LatestHeadReadinessResult {
    pub ready: bool,
    pub failures: Vec<ReadinessFailure>,
    pub latest_lint: Option<SpecLintResultV1>,
}

impl LatestHeadReadinessResult {
    /// Flatten failures using the established readiness error presentation.
    pub fn to_error_string(&self) -> Option<String> {
        ProposalReadinessResult {
            ready: self.ready,
            failures: self.failures.clone(),
        }
        .to_error_string()
    }
}

impl ProposalReadinessResult {
    /// Flatten failures into a single human-readable paragraph.
    pub fn to_error_string(&self) -> Option<String> {
        if self.failures.is_empty() {
            return None;
        }
        let lines: Vec<String> = self
            .failures
            .iter()
            .map(|f| match &f.detail {
                ReadinessFailureDetail::MissingSection { check_name } => {
                    format!("Missing required coverage: {check_name}")
                }
                ReadinessFailureDetail::Generic { message } => message.clone(),
            })
            .collect();
        Some(lines.join("; "))
    }
}

/// One failed readiness check with an exact detail payload.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadinessFailure {
    pub check: ReadinessCheck,
    pub detail: ReadinessFailureDetail,
}

/// Which high-level check failed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessCheck {
    ProblemCoverage,
    ScopeCoverage,
    ObjectiveCoverage,
    TargetCount,
    AcceptanceCriteriaCount,
    Grounding,
    DependenciesCoverage,
    OpenQuestionsRisksCoverage,
    SpecIntegrity,
}

/// Exact failure payload for a given check.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessFailureDetail {
    MissingSection { check_name: String },
    Generic { message: String },
}

// ── Evaluator ──────────────────────────────────────────────────────────────

/// Evaluate proposal readiness deterministically.
///
/// * `body` — the effective proposal body (markdown or MDX).
/// * `acceptance_criteria` — parsed AC items (plain text or structured).
/// * `target_count` — number of target projects attached to the proposal.
pub fn evaluate_proposal_readiness(
    body: &str,
    acceptance_criteria: &[AcceptanceCriterionItem],
    target_count: usize,
) -> ProposalReadinessResult {
    let mut failures = Vec::new();
    let normalized_body = normalize_body(body);

    // 1. Required coverage: problem, scope, objective/outcomes
    if !has_problem_coverage(&normalized_body) {
        failures.push(ReadinessFailure {
            check: ReadinessCheck::ProblemCoverage,
            detail: ReadinessFailureDetail::MissingSection {
                check_name: "problem".to_string(),
            },
        });
    }
    if !has_scope_coverage(&normalized_body) {
        failures.push(ReadinessFailure {
            check: ReadinessCheck::ScopeCoverage,
            detail: ReadinessFailureDetail::MissingSection {
                check_name: "scope".to_string(),
            },
        });
    }
    if !has_objective_coverage(&normalized_body) {
        failures.push(ReadinessFailure {
            check: ReadinessCheck::ObjectiveCoverage,
            detail: ReadinessFailureDetail::MissingSection {
                check_name: "objective / outcomes".to_string(),
            },
        });
    }

    // 2. At least one target
    if target_count == 0 {
        failures.push(ReadinessFailure {
            check: ReadinessCheck::TargetCount,
            detail: ReadinessFailureDetail::Generic {
                message: "At least one target project is required".to_string(),
            },
        });
    }

    // 3. At least one acceptance criterion
    if acceptance_criteria.is_empty() {
        failures.push(ReadinessFailure {
            check: ReadinessCheck::AcceptanceCriteriaCount,
            detail: ReadinessFailureDetail::Generic {
                message: "At least one acceptance criterion is required".to_string(),
            },
        });
    }

    // 4. AC quality (vague / unverifiable / not agent-confirmable) is a
    //    semantic judgment owned by the tribunal Judge (see judge.md
    //    "Definition of Done"); this gate only checks structural presence.

    // 5. Grounding: file-map/code-path block OR named entry points in prose
    if !has_grounding(&normalized_body) {
        failures.push(ReadinessFailure {
            check: ReadinessCheck::Grounding,
            detail: ReadinessFailureDetail::Generic {
                message: "Missing grounding: add a file-map block, code-path fenced block, or named entry points in prose".to_string(),
            },
        });
    }

    // 6. Dependencies / coordination coverage
    if !has_dependencies_coverage(&normalized_body) {
        failures.push(ReadinessFailure {
            check: ReadinessCheck::DependenciesCoverage,
            detail: ReadinessFailureDetail::MissingSection {
                check_name: "dependencies / coordination".to_string(),
            },
        });
    }

    // 7. Open questions / risks coverage
    if !has_open_questions_coverage(&normalized_body) {
        failures.push(ReadinessFailure {
            check: ReadinessCheck::OpenQuestionsRisksCoverage,
            detail: ReadinessFailureDetail::MissingSection {
                check_name: "open questions / risks".to_string(),
            },
        });
    }

    ProposalReadinessResult {
        ready: failures.is_empty(),
        failures,
    }
}

/// Evaluate readiness for the latest material proposal head.
///
/// The history table also contains lightweight lifecycle rows at the current
/// sequence. Resolve by the complete head identity, not sequence alone, before
/// asking the repository for lint. The repository accessor owns cache repair
/// and deterministic recomputation for legacy revisions; this module never
/// invokes a second linter or doctor query.
pub async fn evaluate_latest_head_readiness(
    repo: &ProposalRepository,
    proposal: &djinn_core::models::Proposal,
    acceptance_criteria: &[AcceptanceCriterionItem],
    target_count: usize,
) -> LatestHeadReadinessResult {
    evaluate_latest_head_readiness_for_candidate(
        repo,
        proposal,
        &proposal.body,
        acceptance_criteria,
        target_count,
    )
    .await
}

/// Evaluate candidate structural readiness while loading lint for the current
/// immutable head. Mutation gates use this before a candidate is persisted:
/// the candidate body and acceptance criteria must be validated, while a
/// reachable corrupt current head still fails closed through the same shared
/// repository-backed lint result as every other readiness surface.
pub async fn evaluate_latest_head_readiness_for_candidate(
    repo: &ProposalRepository,
    proposal: &djinn_core::models::Proposal,
    body: &str,
    acceptance_criteria: &[AcceptanceCriterionItem],
    target_count: usize,
) -> LatestHeadReadinessResult {
    let readiness = evaluate_proposal_readiness(body, acceptance_criteria, target_count);
    compose_latest_head_readiness(
        readiness,
        resolve_and_lint_current_head(repo, proposal).await,
    )
}

async fn resolve_and_lint_current_head(
    repo: &ProposalRepository,
    proposal: &djinn_core::models::Proposal,
) -> Result<SpecLintResultV1, String> {
    let revisions = repo
        .revisions(&proposal.id)
        .await
        .map_err(|error| error.to_string())?;
    let revision = revisions
        .iter()
        .rev()
        .find(|revision| {
            revision.event_kind == "spec_revision"
                && revision.seq == proposal.latest_revision_seq
                && revision.body == proposal.body
                && revision.body_format == proposal.body_format
        })
        .ok_or_else(|| {
            format!(
                "current head snapshot unavailable: {}/{} with matching body and format",
                proposal.id, proposal.latest_revision_seq
            )
        })?;
    repo.lint_for_revision(revision)
        .await
        .map_err(|error| error.to_string())
}

fn compose_latest_head_readiness(
    readiness: ProposalReadinessResult,
    lint: Result<SpecLintResultV1, String>,
) -> LatestHeadReadinessResult {
    let mut failures = readiness.failures;
    let latest_lint = match lint {
        Ok(lint) => {
            // `lint_for_revision` guarantees its stable ordering. Preserve that
            // ordering when projecting errors into Definition-of-Ready failures.
            for violation in &lint.errors {
                failures.push(ReadinessFailure {
                    check: ReadinessCheck::SpecIntegrity,
                    detail: ReadinessFailureDetail::Generic {
                        message: format!(
                            "Spec integrity: {} at bytes {}..{}",
                            violation.code, violation.span.start, violation.span.end
                        ),
                    },
                });
            }
            Some(lint)
        }
        Err(error) => {
            failures.push(ReadinessFailure {
                check: ReadinessCheck::SpecIntegrity,
                detail: ReadinessFailureDetail::Generic {
                    message: format!("Spec integrity: unable to load current head lint: {error}"),
                },
            });
            None
        }
    };
    LatestHeadReadinessResult {
        ready: failures.is_empty(),
        failures,
        latest_lint,
    }
}

// ── Normalization ──────────────────────────────────────────────────────────

fn normalize_body(body: &str) -> String {
    body.to_lowercase()
}

// ── Section coverage heuristics ────────────────────────────────────────────

fn has_problem_coverage(normalized: &str) -> bool {
    has_heading_family(
        normalized,
        &[
            "problem",
            "problem statement",
            "motivation",
            "why",
            "background",
        ],
    ) || has_inline_keyword_family(
        normalized,
        &["problem:", "problem statement", "the problem is"],
    )
}

fn has_scope_coverage(normalized: &str) -> bool {
    has_heading_family(
        normalized,
        &["scope", "in scope", "out of scope", "boundaries"],
    ) || has_inline_keyword_family(normalized, &["scope:", "in scope", "out of scope"])
}

fn has_objective_coverage(normalized: &str) -> bool {
    has_heading_family(
        normalized,
        &[
            "objective",
            "objectives",
            "outcomes",
            "goals",
            "success criteria",
            "deliverables",
        ],
    ) || has_inline_keyword_family(
        normalized,
        &[
            "objective:",
            "objectives:",
            "outcomes:",
            "goals:",
            "success criteria:",
        ],
    )
}

fn has_dependencies_coverage(normalized: &str) -> bool {
    has_heading_family(
        normalized,
        &[
            "dependencies",
            "dependency",
            "coordination",
            "blocked by",
            "prerequisites",
            "requires",
            "related work",
        ],
    ) || has_inline_keyword_family(
        normalized,
        &[
            "dependencies:",
            "dependency:",
            "coordination:",
            "prerequisites:",
            "blocked by:",
            "related work:",
        ],
    )
}

fn has_open_questions_coverage(normalized: &str) -> bool {
    has_heading_family(
        normalized,
        &[
            "open questions",
            "risks",
            "risk",
            "assumptions",
            "unknowns",
            "mitigations",
        ],
    ) || has_inline_keyword_family(
        normalized,
        &["open questions:", "risks:", "assumptions:", "unknowns:"],
    )
}

// ── Heading / keyword helpers ──────────────────────────────────────────────

fn has_heading_family(normalized: &str, keywords: &[&str]) -> bool {
    for line in normalized.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("#") {
            let rest = rest.trim_start_matches('#').trim_start();
            for kw in keywords {
                // Match the keyword as a whole word (or whole phrase) anywhere in
                // the heading, not just as a prefix.  This lets headings like
                // "Dependency and coordination plan" satisfy the dependencies
                // family without matching unrelated prose (e.g. "risky" must not
                // match the "risk" keyword).
                if heading_contains_word(rest, kw) {
                    return true;
                }
            }
        }
    }
    false
}

/// True when `keyword` appears in `heading` bounded by word boundaries on both
/// sides.  `keyword` may itself be a multi-word phrase (e.g. "out of scope");
/// only its outer edges are boundary-checked.
fn heading_contains_word(heading: &str, keyword: &str) -> bool {
    if keyword.is_empty() {
        return false;
    }
    let mut search_from = 0;
    while let Some(pos) = heading[search_from..].find(keyword) {
        let start = search_from + pos;
        let end = start + keyword.len();
        let before_ok = heading[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = heading[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        // Advance past this occurrence to look for a bounded match later on.
        search_from = start + 1;
    }
    false
}

fn has_inline_keyword_family(normalized: &str, keywords: &[&str]) -> bool {
    for kw in keywords {
        if normalized.contains(kw) {
            return true;
        }
    }
    false
}

// ── Grounding heuristic ────────────────────────────────────────────────────

fn has_grounding(normalized: &str) -> bool {
    // File-map / code-path fenced block markers
    if normalized.contains("```file-map")
        || normalized.contains("```code-path")
        || normalized.contains("```paths")
        || normalized.contains("```filemap")
    {
        return true;
    }
    // Named entry points in prose: path-like strings or explicit "entry point" mentions
    if normalized.contains("entry point")
        || normalized.contains("entrypoint")
        || normalized.contains("entry points")
    {
        return true;
    }
    // Path-like markers: `src/` or `path/to/` or `*.rs` etc.
    if normalized.contains("src/")
        || normalized.contains("path/to/")
        || normalized.contains("file:")
        || normalized.contains("files:")
    {
        return true;
    }
    false
}

// ── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, ProposalCreateInput,
        test_support::{
            delete_proposal_lint_results_for_test, replace_legacy_proposal_head_for_test,
        },
    };

    const READY_BODY: &str = r#"
# Problem
Users cannot do X.
# Scope
In scope: Y.
# Objectives
Deliver A.
# Dependencies
None.
# Open Questions
What if D fails?
Entry points: src/main.rs.
"#;

    async fn test_repo_and_proposal(
        body: &str,
        body_format: Option<&str>,
    ) -> (Database, ProposalRepository, djinn_core::models::Proposal) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Latest head readiness",
                body,
                acceptance_criteria: Some("[\"The result is testable\"]"),
                status: None,
                body_format,
            })
            .await
            .unwrap();
        (db, repo, proposal)
    }

    fn ac_text(text: &str) -> AcceptanceCriterionItem {
        AcceptanceCriterionItem::Text(text.to_string())
    }

    #[test]
    fn passing_body() {
        let body = r#"
# Problem
Users cannot do X.

# Scope
In scope: Y. Out of scope: Z.

# Objectives
- Deliver A
- Deliver B

## File map
```file-map
src/main.rs
src/lib.rs
```

# Dependencies
Blocked by service C.

# Open Questions
What happens if D fails?
"#;
        let acs = vec![ac_text("API returns 200")];
        let result = evaluate_proposal_readiness(body, &acs, 1);
        assert!(result.ready, "expected ready: {:?}", result.failures);
        assert!(result.failures.is_empty());
    }

    #[test]
    fn missing_sections() {
        let body = "Just some random text.";
        let acs = vec![ac_text("API returns 200")];
        let result = evaluate_proposal_readiness(body, &acs, 1);
        assert!(!result.ready);
        let checks: Vec<_> = result.failures.iter().map(|f| f.check.clone()).collect();
        assert!(checks.contains(&ReadinessCheck::ProblemCoverage));
        assert!(checks.contains(&ReadinessCheck::ScopeCoverage));
        assert!(checks.contains(&ReadinessCheck::ObjectiveCoverage));
        assert!(checks.contains(&ReadinessCheck::Grounding));
        assert!(checks.contains(&ReadinessCheck::DependenciesCoverage));
        assert!(checks.contains(&ReadinessCheck::OpenQuestionsRisksCoverage));
    }

    #[test]
    fn no_targets() {
        let body = r#"
# Problem
Users cannot do X.
# Scope
In scope: Y.
# Objectives
Deliver A.
# Dependencies
None.
# Open Questions
What if D fails?
## File map
```file-map
src/main.rs
```
"#;
        let acs = vec![ac_text("API returns 200")];
        let result = evaluate_proposal_readiness(body, &acs, 0);
        assert!(!result.ready);
        assert!(
            result
                .failures
                .iter()
                .any(|f| f.check == ReadinessCheck::TargetCount)
        );
    }

    #[test]
    fn no_ac() {
        let body = r#"
# Problem
Users cannot do X.
# Scope
In scope: Y.
# Objectives
Deliver A.
# Dependencies
None.
# Open Questions
What if D fails?
## File map
```file-map
src/main.rs
```
"#;
        let result = evaluate_proposal_readiness(body, &[], 1);
        assert!(!result.ready);
        assert!(
            result
                .failures
                .iter()
                .any(|f| f.check == ReadinessCheck::AcceptanceCriteriaCount)
        );
    }

    #[test]
    fn grounding_via_code_paths() {
        let body = r#"
# Problem
Users cannot do X.
# Scope
In scope: Y.
# Objectives
Deliver A.
# Dependencies
None.
# Open Questions
What if D fails?
Entry points: src/main.rs and src/lib.rs.
"#;
        let acs = vec![ac_text("API returns 200")];
        let result = evaluate_proposal_readiness(body, &acs, 1);
        assert!(result.ready, "expected ready: {:?}", result.failures);
    }

    #[test]
    fn dependency_and_coordination_plan_heading_passes() {
        // Regression: the real heading "## Dependency and coordination plan"
        // previously failed because prefix matching required the heading to
        // start with the plural "dependencies" or "coordination".
        let normalized = normalize_body("## Dependency and coordination plan\nBody.");
        assert!(
            has_dependencies_coverage(&normalized),
            "Dependency and coordination plan heading must satisfy dependencies coverage"
        );
    }

    #[test]
    fn dependencies_coverage_via_full_body() {
        let body = r#"
# Problem
Users cannot do X.
# Scope
In scope: Y.
# Objectives
Deliver A.
## Dependency and coordination plan
This work depends on service C landing first.
# Open Questions
What if D fails?
Entry points: src/main.rs.
"#;
        let acs = vec![ac_text("API returns 200")];
        let result = evaluate_proposal_readiness(body, &acs, 1);
        assert!(result.ready, "expected ready: {:?}", result.failures);
    }

    #[test]
    fn heading_word_matching_does_not_over_loosen() {
        // Adversarial: substrings of keywords must NOT satisfy coverage.
        // "risky" contains "risk" but is not a whole-word match.
        let normalized = normalize_body("## Risky ventures\nSome prose.");
        assert!(
            !has_open_questions_coverage(&normalized),
            "'Risky ventures' heading must not satisfy open-questions/risks coverage"
        );

        // A heading unrelated to dependencies must not match despite sharing
        // letters ("dependency"/"dependencies" not present as words).
        let normalized = normalize_body("## Implementation notes\nDetails here.");
        assert!(
            !has_dependencies_coverage(&normalized),
            "'Implementation notes' heading must not satisfy dependencies coverage"
        );
    }

    #[test]
    fn heading_contains_word_boundaries() {
        assert!(heading_contains_word(
            "dependency and coordination plan",
            "dependency"
        ));
        assert!(heading_contains_word(
            "dependency and coordination plan",
            "coordination"
        ));
        assert!(heading_contains_word(
            "out of scope details",
            "out of scope"
        ));
        // Boundary rejections.
        assert!(!heading_contains_word("risky ventures", "risk"));
        assert!(!heading_contains_word("dependencies", "dependency"));
        assert!(!heading_contains_word("scoped access", "scope"));
    }

    #[test]
    fn to_error_string_format() {
        let result = ProposalReadinessResult {
            ready: false,
            failures: vec![ReadinessFailure {
                check: ReadinessCheck::ProblemCoverage,
                detail: ReadinessFailureDetail::MissingSection {
                    check_name: "problem".to_string(),
                },
            }],
        };
        let msg = result.to_error_string().unwrap();
        assert!(msg.contains("Missing required coverage: problem"));
    }

    #[tokio::test]
    async fn latest_head_readiness_keeps_clean_lint_and_ordinary_readiness() {
        let (_db, repo, proposal) = test_repo_and_proposal(READY_BODY, None).await;
        let acs = vec![ac_text("The result is testable")];
        let result = evaluate_latest_head_readiness(&repo, &proposal, &acs, 1).await;

        assert!(result.ready, "unexpected failures: {:?}", result.failures);
        assert!(result.failures.is_empty());
        assert!(result.latest_lint.is_some());
        assert!(result.latest_lint.unwrap().errors.is_empty());
    }

    #[tokio::test]
    async fn candidate_readiness_keeps_candidate_body_and_ac_checks() {
        let (_db, repo, proposal) = test_repo_and_proposal(READY_BODY, None).await;
        let result = evaluate_latest_head_readiness_for_candidate(
            &repo,
            &proposal,
            "candidate body without required coverage",
            &[],
            1,
        )
        .await;

        assert!(!result.ready);
        assert!(
            result.latest_lint.is_some(),
            "current-head lint is retained"
        );
        assert!(result.failures.iter().any(|failure| {
            failure.check == ReadinessCheck::ProblemCoverage
                || failure.check == ReadinessCheck::AcceptanceCriteriaCount
        }));
    }

    #[tokio::test]
    async fn latest_head_readiness_exposes_warnings_without_blocking() {
        let body = format!("{READY_BODY}\n[dangling](#missing-anchor)\n");
        let (_db, repo, proposal) = test_repo_and_proposal(&body, None).await;
        let acs = vec![ac_text("The result is testable")];
        let result = evaluate_latest_head_readiness(&repo, &proposal, &acs, 1).await;

        assert!(
            result.ready,
            "warnings must not block: {:?}",
            result.failures
        );
        assert!(result.failures.is_empty());
        assert!(!result.latest_lint.unwrap().warnings.is_empty());
    }

    #[tokio::test]
    async fn latest_head_readiness_recomputes_uncached_legacy_corrupt_head() {
        let (db, repo, proposal) = test_repo_and_proposal(READY_BODY, Some("mdx")).await;
        let corrupt_body = format!(
            "{READY_BODY}\n<Callout id=\"duplicate\">one</Callout>\n<Callout id=\"duplicate\">two</Callout>"
        );
        // Simulate a legacy material head written before repository-boundary
        // linting, with no persisted lint result.
        replace_legacy_proposal_head_for_test(&db, &proposal.id, &corrupt_body, "mdx").await;
        delete_proposal_lint_results_for_test(&db, &proposal.id).await;
        let lint_row_count = repo.lint_result_count(&proposal.id).await.unwrap();
        assert_eq!(
            lint_row_count, 0,
            "legacy head starts without persisted lint"
        );
        let proposal = repo.get(&proposal.id).await.unwrap().unwrap();
        let acs = vec![ac_text("The result is testable")];
        let result = evaluate_latest_head_readiness(&repo, &proposal, &acs, 1).await;

        let lint = result
            .latest_lint
            .as_ref()
            .expect("recomputed lint summary");
        assert!(!lint.errors.is_empty());
        assert!(!result.ready);
        let integrity_messages: Vec<_> = result
            .failures
            .iter()
            .filter(|failure| failure.check == ReadinessCheck::SpecIntegrity)
            .map(|failure| match &failure.detail {
                ReadinessFailureDetail::Generic { message } => message.clone(),
                detail => panic!("unexpected integrity detail: {detail:?}"),
            })
            .collect();
        assert_eq!(integrity_messages.len(), lint.errors.len());
        assert_eq!(
            integrity_messages,
            lint.errors
                .iter()
                .map(|violation| format!(
                    "Spec integrity: {} at bytes {}..{}",
                    violation.code, violation.span.start, violation.span.end
                ))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn latest_head_readiness_ignores_same_sequence_status_change_row() {
        let (_db, repo, proposal) = test_repo_and_proposal(READY_BODY, None).await;
        repo.advance_draft_to_in_review(&proposal.id).await.unwrap();
        let revisions = repo.revisions(&proposal.id).await.unwrap();
        assert!(revisions.iter().any(|revision| {
            revision.event_kind == "status_change"
                && revision.seq == proposal.latest_revision_seq
                && revision.body == proposal.body
                && revision.body_format == proposal.body_format
        }));
        let acs = vec![ac_text("The result is testable")];
        let result = evaluate_latest_head_readiness(&repo, &proposal, &acs, 1).await;

        assert!(
            result.ready,
            "lifecycle row replaced head: {:?}",
            result.failures
        );
        assert_eq!(
            result.latest_lint.unwrap().body_sha256,
            djinn_spec_lint::body_sha256(READY_BODY)
        );
    }

    #[tokio::test]
    async fn latest_head_readiness_fails_closed_when_lint_cannot_load() {
        let (db, repo, proposal) = test_repo_and_proposal(READY_BODY, None).await;
        replace_legacy_proposal_head_for_test(&db, &proposal.id, READY_BODY, "unknown").await;
        let proposal = repo.get(&proposal.id).await.unwrap().unwrap();
        let acs = vec![ac_text("The result is testable")];
        let result = evaluate_latest_head_readiness(&repo, &proposal, &acs, 1).await;

        assert!(!result.ready);
        assert!(result.latest_lint.is_none());
        assert!(
            result
                .to_error_string()
                .unwrap()
                .contains("unable to load current head lint")
        );
    }
}
