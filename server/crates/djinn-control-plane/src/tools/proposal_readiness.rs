use crate::tools::epic_ops::AcceptanceCriterionItem;
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
}
