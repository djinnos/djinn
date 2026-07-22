//! Phase 2 QA-pair extraction from resolved `pitfall` and `case` corpus notes.
//!
//! This module mechanically extracts question–answer pairs from Phase 1
//! [`CorpusNoteRow`] records whose note type is `pitfall` or `case` and status
//! is `active`. The extraction reuses [`djinn_db::repositories::note::required_sections`]
//! as the canonical section-name source and produces deterministic,
//! serde-friendly QA fixtures for downstream judge/report slices.
//!
//! # Extraction rules
//!
//! | Note type | Question sections (symptom / failure / situation) | Gold sections (answer) |
//! |-----------|--------------------------------------------------|------------------------|
//! | `pitfall` | `Trigger / smell`, `Failure mode`, `Observable symptoms` | `Prevention`, `Recovery` |
//! | `case`    | `Situation`, `Constraint` | `Approach taken`, `Reusable lesson` |
//!
//! Notes missing any required section body are skipped with a diagnostic
//! reason rather than producing a degraded QA pair. No LLM or external
//! network call is made.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use djinn_db::repositories::note::required_sections;

use crate::fixtures::CorpusNoteRow;

// ── QA data types ──────────────────────────────────────────────────────────

/// A single gold section extracted from a note body.
///
/// Contains the section heading label and its trimmed body text. These are
/// mechanically parsed from `## {label}` markdown headings in the note
/// content — no LLM is involved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QaGoldSection {
    /// The section heading label (e.g. `"Prevention"`, `"Recovery"`).
    pub label: String,
    /// Trimmed body text under the heading, up to the next `## ` heading or
    /// end of content.
    pub body: String,
}

/// A Phase 2 QA pair extracted from a resolved `pitfall` or `case` note.
///
/// Each pair carries stable identifiers, note metadata, a deterministic
/// question text (derived from symptom/situation sections), and a gold answer
/// (derived from prevention/recovery/approach sections).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QaPair {
    /// Stable, deterministic QA identifier derived from the source note
    /// permalink and note type. Format: `qa-{note_type}-{sha256(permalink)[..12]}`.
    pub qa_id: String,
    /// Permalink of the source note in the corpus.
    pub source_permalink: String,
    /// Human-readable title of the source note.
    pub source_title: String,
    /// Note type: `"pitfall"` or `"case"`.
    pub note_type: String,
    /// Lifecycle status of the source note (`"active"`, `"archived"`, etc.).
    pub source_status: String,
    /// Question text assembled from symptom/situation sections. For `pitfall`
    /// this is the concatenation of `Trigger / smell`, `Failure mode`, and
    /// `Observable symptoms` bodies. For `case` this is the concatenation of
    /// `Situation` and `Constraint` bodies.
    pub question: String,
    /// Gold answer text assembled from prevention/recovery/approach sections.
    /// For `pitfall` this is the concatenation of `Prevention` and `Recovery`
    /// bodies. For `case` this is the concatenation of `Approach taken` and
    /// `Reusable lesson` bodies.
    pub gold_answer: String,
    /// Sections that contributed to the question text.
    pub question_sections: Vec<QaGoldSection>,
    /// Sections that contributed to the gold answer.
    pub answer_sections: Vec<QaGoldSection>,
    /// ISO-8601 creation timestamp of the source note.
    pub created_at: String,
    /// ISO-8601 last-updated timestamp of the source note.
    pub updated_at: String,
    /// ISO-8601 last-accessed timestamp of the source note.
    pub last_accessed: String,
    /// Age of the note in days from creation to last update (rounded down).
    pub age_days: i64,
    /// Bayesian confidence score of the source note (0.0–1.0).
    pub confidence: f64,
}

/// Summary of the QA extraction run over a set of corpus notes.
///
/// Tracks successes, skips (notes missing required section bodies), and
/// total eligible notes examined.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QaExtractionReport {
    /// Successfully extracted QA pairs.
    pub pairs: Vec<QaPair>,
    /// Notes that were skipped with a reason string.
    pub skipped: Vec<QaSkip>,
    /// Total notes examined that were eligible (pitfall/case with active status).
    pub eligible_count: usize,
}

/// A note that was skipped during QA extraction with a diagnostic reason.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QaSkip {
    /// Permalink of the skipped note.
    pub permalink: String,
    /// Note type of the skipped note.
    pub note_type: String,
    /// Why the note was skipped.
    pub reason: String,
}

// ── Section extraction helpers ─────────────────────────────────────────────

/// Sections whose body text contributes to the **question** for a pitfall note.
const PITFALL_QUESTION_SECTIONS: &[&str] =
    &["Trigger / smell", "Failure mode", "Observable symptoms"];

/// Sections whose body text contributes to the **gold answer** for a pitfall note.
const PITFALL_ANSWER_SECTIONS: &[&str] = &["Prevention", "Recovery"];

/// Sections whose body text contributes to the **question** for a case note.
const CASE_QUESTION_SECTIONS: &[&str] = &["Situation", "Constraint"];

/// Sections whose body text contributes to the **gold answer** for a case note.
const CASE_ANSWER_SECTIONS: &[&str] = &["Approach taken", "Reusable lesson"];

/// Extract the body text under a `## {heading}` markdown section.
///
/// Returns `None` if the heading is not found or the section body is empty
/// (only whitespace). The heading match is case-sensitive and exact.
///
/// Body text is trimmed of leading/trailing whitespace and includes all lines
/// until the next `## ` heading or end of content.
fn extract_section_body<'a>(content: &'a str, heading: &str) -> Option<String> {
    let full_heading = format!("## {heading}");
    let mut in_section = false;
    let mut body_lines: Vec<&'a str> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim_end();

        if trimmed == full_heading {
            // We hit the target heading. Flush any previously collected body
            // (shouldn't happen for correct in-order content, but defensive).
            in_section = true;
            body_lines.clear();
            continue;
        }

        if in_section {
            // A new `## ` heading ends the current section.
            if trimmed.starts_with("## ") {
                break;
            }
            body_lines.push(line);
        }
    }

    if !in_section {
        return None;
    }

    let body = body_lines.join("\n").trim().to_string();
    if body.is_empty() { None } else { Some(body) }
}

/// Build a deterministic QA ID from the note type and permalink.
///
/// Format: `qa-{note_type}-{sha256(permalink)[..12]}` where the hash is
/// lower-case hex.
fn stable_qa_id(note_type: &str, permalink: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(permalink.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    format!("qa-{note_type}-{}", &hash[..12])
}

/// Compute the age in whole days between two ISO-8601 timestamps.
///
/// This is a simple string-prefix-based day difference using `YYYY-MM-DD`
/// parsing. It is intentionally deterministic and does not depend on any
/// date-time crate. Returns 0 if parsing fails or the dates are identical.
fn age_days(created_at: &str, updated_at: &str) -> i64 {
    fn parse_ymd(s: &str) -> Option<(i32, u32, u32)> {
        let date_part = &s[..10.min(s.len())];
        let mut parts = date_part.split('-');
        let y: i32 = parts.next()?.parse().ok()?;
        let m: u32 = parts.next()?.parse().ok()?;
        let d: u32 = parts.next()?.parse().ok()?;
        Some((y, m, d))
    }

    fn to_julian_day(y: i32, m: u32, d: u32) -> i64 {
        // Fliegel–Van Flandern algorithm
        let a = (14 - m) / 12;
        let y2 = y as i64 + 4800 - a as i64;
        let m2 = m as i64 + 12 * a as i64 - 3;
        d as i64 + (153 * m2 + 2) / 5 + 365 * y2 + y2 / 4 - y2 / 100 + y2 / 400 - 32045
    }

    match (parse_ymd(created_at), parse_ymd(updated_at)) {
        (Some((cy, cm, cd)), Some((uy, um, ud))) => {
            let c_jd = to_julian_day(cy, cm, cd);
            let u_jd = to_julian_day(uy, um, ud);
            (u_jd - c_jd).max(0)
        }
        _ => 0,
    }
}

// ── Public extraction API ──────────────────────────────────────────────────

/// Extract QA pairs from all eligible corpus notes.
///
/// A note is **eligible** when:
/// - `note_type` is `"pitfall"` or `"case"`
/// - `status` is `"active"`
///
/// A note is **skipped** when:
/// - It is not eligible (not pitfall/case, or non-active status)
/// - Any required question section has an empty or missing body
/// - Any required answer section has an empty or missing body
///
/// Extraction is fully deterministic: no LLM calls, no network calls, no
/// randomness.
pub fn extract_qa_pairs(corpus: &[CorpusNoteRow]) -> QaExtractionReport {
    let mut report = QaExtractionReport::default();

    for note in corpus {
        // Only pitfall/case with active status are eligible.
        if !matches!(note.note_type.as_str(), "pitfall" | "case") {
            continue;
        }
        if note.status != "active" {
            report.skipped.push(QaSkip {
                permalink: note.permalink.clone(),
                note_type: note.note_type.clone(),
                reason: format!("status '{}' is not active", note.status),
            });
            continue;
        }
        report.eligible_count += 1;

        let (question_sections_labels, answer_sections_labels) = match note.note_type.as_str() {
            "pitfall" => (PITFALL_QUESTION_SECTIONS, PITFALL_ANSWER_SECTIONS),
            "case" => (CASE_QUESTION_SECTIONS, CASE_ANSWER_SECTIONS),
            _ => unreachable!("filtered above"),
        };

        // Validate that `required_sections()` agrees with our section sets.
        let required = required_sections(&note.note_type);
        let required_set: std::collections::HashSet<&str> = required.iter().copied().collect();

        // Check that all question and answer section labels are in required_sections.
        // If any are missing from `required_sections`, that's a code-level contract
        // violation — we skip the note and log a diagnostic.
        for &label in question_sections_labels
            .iter()
            .chain(answer_sections_labels.iter())
        {
            if !required_set.contains(label) {
                report.skipped.push(QaSkip {
                    permalink: note.permalink.clone(),
                    note_type: note.note_type.clone(),
                    reason: format!(
                        "section '{label}' not in required_sections for '{}'",
                        note.note_type
                    ),
                });
            }
        }

        // Extract question sections.
        let mut question_sections = Vec::new();
        let mut missing_question = None;
        for &label in question_sections_labels {
            match extract_section_body(&note.content, label) {
                Some(body) => question_sections.push(QaGoldSection {
                    label: label.to_string(),
                    body,
                }),
                None => {
                    missing_question = Some(label);
                    break;
                }
            }
        }

        if let Some(missing) = missing_question {
            report.skipped.push(QaSkip {
                permalink: note.permalink.clone(),
                note_type: note.note_type.clone(),
                reason: format!("missing or empty question section: '{missing}'"),
            });
            continue;
        }

        // Extract answer sections.
        let mut answer_sections = Vec::new();
        let mut missing_answer = None;
        for &label in answer_sections_labels {
            match extract_section_body(&note.content, label) {
                Some(body) => answer_sections.push(QaGoldSection {
                    label: label.to_string(),
                    body,
                }),
                None => {
                    missing_answer = Some(label);
                    break;
                }
            }
        }

        if let Some(missing) = missing_answer {
            report.skipped.push(QaSkip {
                permalink: note.permalink.clone(),
                note_type: note.note_type.clone(),
                reason: format!("missing or empty answer section: '{missing}'"),
            });
            continue;
        }

        // Build question and gold answer text by joining section bodies with
        // a double-newline separator for readability.
        let question = question_sections
            .iter()
            .map(|s| s.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        let gold_answer = answer_sections
            .iter()
            .map(|s| s.body.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        let qa_id = stable_qa_id(&note.note_type, &note.permalink);

        report.pairs.push(QaPair {
            qa_id,
            source_permalink: note.permalink.clone(),
            source_title: note.title.clone(),
            note_type: note.note_type.clone(),
            source_status: note.status.clone(),
            question,
            gold_answer,
            question_sections,
            answer_sections,
            created_at: note.timestamps.created_at.clone(),
            updated_at: note.timestamps.updated_at.clone(),
            last_accessed: note.timestamps.last_accessed.clone(),
            age_days: age_days(&note.timestamps.created_at, &note.timestamps.updated_at),
            confidence: note.confidence,
        });
    }

    report
}

/// Convenience: extract QA pairs and return only the successfully extracted
/// pairs, discarding the extraction report metadata.
pub fn extract_qa_pairs_only(corpus: &[CorpusNoteRow]) -> Vec<QaPair> {
    extract_qa_pairs(corpus).pairs
}

// ── Unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use crate::fixtures::LifecycleTimestamps;

    /// Minimal well-formed pitfall corpus note with all required sections.
    fn pitfall_note_fixture() -> CorpusNoteRow {
        let content = r#"## Trigger / smell

The agent emits a slot lifecycle timeout error when a model slot is released while callbacks are still pending.

## Failure mode

The supervisor panics with a guard violation because the slot status is `Released` but the callback expects `Active`.

## Observable symptoms

- Log lines contain `SlotStatus::Released guard violation`
- The agent crashes with exit code 137
- Slot metrics show premature release counts

## Prevention

Always check `slot.status()` before dispatching async callbacks. Use the `with_slot_guard` helper to hold the slot alive for the callback's lifetime.

## Recovery

Restart the slot supervisor with `--reset-sessions`. The guard violation is non-fatal if caught by the outer supervision tree.

## Related

- patterns/supervisor-guard
- cases/slot-lifecycle-race"#;

        CorpusNoteRow {
            permalink: "pitfalls/slot-guard-violation".to_string(),
            title: "Slot guard violation pitfall".to_string(),
            content: content.to_string(),
            note_type: "pitfall".to_string(),
            folder: "pitfalls".to_string(),
            status: "active".to_string(),
            tags: vec!["slot".to_string(), "guard".to_string()],
            retrieval_anchor: Some("slot guard violation during callback dispatch".to_string()),
            timestamps: LifecycleTimestamps {
                created_at: "2026-06-01T10:00:00.000Z".to_string(),
                updated_at: "2026-07-01T10:00:00.000Z".to_string(),
                last_accessed: "2026-07-10T10:00:00.000Z".to_string(),
            },
            confidence: 0.9,
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        }
    }

    /// Minimal well-formed case corpus note with all required sections.
    fn case_note_fixture() -> CorpusNoteRow {
        let content = r#"## Situation

A memory retrieval query returns stale notes because the Bayesian confidence decay rate is too aggressive for recently-updated notes.

## Constraint

The decay function must not change the scoring contract for notes older than 90 days; only the 0–30 day window can be adjusted.

## Approach taken

Introduced a tiered decay curve: linear for the first 30 days, exponential after. Updated the `decay_signal_for_elapsed_days` function to accept a tier boundary parameter.

## Result

Post-change, notes updated within 7 days now rank in the top 5 for relevant queries where they previously dropped to rank 15+. No regression for the >90 day cohort.

## Why it worked / failed

The tiered approach preserved the tail behavior that downstream consumers depend on while protecting recently-updated notes from premature demotion.

## Reusable lesson

When adjusting decay functions, always tier the curve so the long tail is preserved. Test against the >90 day cohort as a regression guard.

## Related

- decisions/decay-rate-adrs
- pitfalls/over-decay"#;

        CorpusNoteRow {
            permalink: "cases/decay-rate-adjustment".to_string(),
            title: "Decay rate tiered adjustment case".to_string(),
            content: content.to_string(),
            note_type: "case".to_string(),
            folder: "cases".to_string(),
            status: "active".to_string(),
            tags: vec!["decay".to_string(), "scoring".to_string()],
            retrieval_anchor: None,
            timestamps: LifecycleTimestamps {
                created_at: "2026-05-15T00:00:00.000Z".to_string(),
                updated_at: "2026-07-10T00:00:00.000Z".to_string(),
                last_accessed: "2026-07-10T00:00:00.000Z".to_string(),
            },
            confidence: 0.85,
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        }
    }

    /// A pitfall note missing the Prevention section body.
    fn pitfall_missing_prevention_fixture() -> CorpusNoteRow {
        let content = r#"## Trigger / smell

The trigger text is present.

## Failure mode

The failure mode text is present.

## Observable symptoms

The symptoms are present.

## Prevention

## Recovery

Recovery text is present.

## Related

- some/related-note"#;

        CorpusNoteRow {
            permalink: "pitfalls/missing-prevention".to_string(),
            title: "Missing prevention pitfall".to_string(),
            content: content.to_string(),
            note_type: "pitfall".to_string(),
            folder: "pitfalls".to_string(),
            status: "active".to_string(),
            tags: vec![],
            retrieval_anchor: None,
            timestamps: LifecycleTimestamps {
                created_at: "2026-01-01T00:00:00.000Z".to_string(),
                updated_at: "2026-01-01T00:00:00.000Z".to_string(),
                last_accessed: "2026-01-01T00:00:00.000Z".to_string(),
            },
            confidence: 1.0,
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        }
    }

    /// A case note missing the entire Reusable lesson section.
    fn case_missing_reusable_lesson_fixture() -> CorpusNoteRow {
        let content = r#"## Situation

A situation description.

## Constraint

A constraint description.

## Approach taken

An approach description.

## Result

A result description.

## Why it worked / failed

A rationale description.

## Related

- some/related-note"#;

        CorpusNoteRow {
            permalink: "cases/missing-lesson".to_string(),
            title: "Missing reusable lesson case".to_string(),
            content: content.to_string(),
            note_type: "case".to_string(),
            folder: "cases".to_string(),
            status: "active".to_string(),
            tags: vec![],
            retrieval_anchor: None,
            timestamps: LifecycleTimestamps {
                created_at: "2026-01-01T00:00:00.000Z".to_string(),
                updated_at: "2026-01-01T00:00:00.000Z".to_string(),
                last_accessed: "2026-01-01T00:00:00.000Z".to_string(),
            },
            confidence: 1.0,
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        }
    }

    #[test]
    fn pitfall_extraction_produces_correct_qa_pair() {
        let note = pitfall_note_fixture();
        let report = extract_qa_pairs(&[note]);

        assert_eq!(report.pairs.len(), 1, "should extract one QA pair");
        assert!(
            report.skipped.is_empty(),
            "should not skip: {:?}",
            report.skipped
        );
        assert_eq!(report.eligible_count, 1);

        let qa = &report.pairs[0];
        assert_eq!(qa.source_permalink, "pitfalls/slot-guard-violation");
        assert_eq!(qa.source_title, "Slot guard violation pitfall");
        assert_eq!(qa.note_type, "pitfall");
        assert_eq!(qa.source_status, "active");
        assert_eq!(qa.confidence, 0.9);

        // QA ID is stable
        assert!(qa.qa_id.starts_with("qa-pitfall-"));
        assert_eq!(qa.qa_id.len(), 23); // "qa-pitfall-" (11) + 12 hash chars

        // Question should contain all three symptom sections
        assert!(
            qa.question.contains("slot lifecycle timeout"),
            "question missing trigger: {}",
            qa.question
        );
        assert!(
            qa.question.contains("supervisor panics"),
            "question missing failure mode: {}",
            qa.question
        );
        assert!(
            qa.question.contains("SlotStatus::Released"),
            "question missing symptoms: {}",
            qa.question
        );

        // Question sections
        assert_eq!(qa.question_sections.len(), 3);
        assert_eq!(qa.question_sections[0].label, "Trigger / smell");
        assert_eq!(qa.question_sections[1].label, "Failure mode");
        assert_eq!(qa.question_sections[2].label, "Observable symptoms");

        // Gold answer should contain prevention and recovery
        assert!(
            qa.gold_answer.contains("with_slot_guard"),
            "answer missing prevention: {}",
            qa.gold_answer
        );
        assert!(
            qa.gold_answer.contains("Restart the slot supervisor"),
            "answer missing recovery: {}",
            qa.gold_answer
        );

        // Answer sections
        assert_eq!(qa.answer_sections.len(), 2);
        assert_eq!(qa.answer_sections[0].label, "Prevention");
        assert_eq!(qa.answer_sections[1].label, "Recovery");

        // Age: 30 days from 2026-06-01 to 2026-07-01
        assert_eq!(qa.age_days, 30);

        // Timestamps passed through
        assert_eq!(qa.created_at, "2026-06-01T10:00:00.000Z");
        assert_eq!(qa.updated_at, "2026-07-01T10:00:00.000Z");
        assert_eq!(qa.last_accessed, "2026-07-10T10:00:00.000Z");
    }

    #[test]
    fn case_extraction_produces_correct_qa_pair() {
        let note = case_note_fixture();
        let report = extract_qa_pairs(&[note]);

        assert_eq!(report.pairs.len(), 1, "should extract one QA pair");
        assert!(
            report.skipped.is_empty(),
            "should not skip: {:?}",
            report.skipped
        );
        assert_eq!(report.eligible_count, 1);

        let qa = &report.pairs[0];
        assert_eq!(qa.source_permalink, "cases/decay-rate-adjustment");
        assert_eq!(qa.note_type, "case");

        // Question: Situation + Constraint
        assert!(
            qa.question.contains("stale notes"),
            "question missing situation: {}",
            qa.question
        );
        assert!(
            qa.question.contains("0–30 day window"),
            "question missing constraint: {}",
            qa.question
        );

        assert_eq!(qa.question_sections.len(), 2);
        assert_eq!(qa.question_sections[0].label, "Situation");
        assert_eq!(qa.question_sections[1].label, "Constraint");

        // Gold answer: Approach taken + Reusable lesson
        assert!(
            qa.gold_answer.contains("tiered decay curve"),
            "answer missing approach: {}",
            qa.gold_answer
        );
        assert!(
            qa.gold_answer.contains("always tier the curve"),
            "answer missing lesson: {}",
            qa.gold_answer
        );

        assert_eq!(qa.answer_sections.len(), 2);
        assert_eq!(qa.answer_sections[0].label, "Approach taken");
        assert_eq!(qa.answer_sections[1].label, "Reusable lesson");

        // Age: 56 days from 2026-05-15 to 2026-07-10
        assert_eq!(qa.age_days, 56);

        // Stable ID
        assert!(qa.qa_id.starts_with("qa-case-"));
    }

    #[test]
    fn pitfall_missing_prevention_is_skipped() {
        let note = pitfall_missing_prevention_fixture();
        let report = extract_qa_pairs(&[note]);

        assert_eq!(report.pairs.len(), 0, "should not produce a QA pair");
        assert_eq!(report.eligible_count, 1, "note is eligible even if skipped");
        assert_eq!(report.skipped.len(), 1, "should skip exactly one note");

        let skip = &report.skipped[0];
        assert_eq!(skip.permalink, "pitfalls/missing-prevention");
        assert_eq!(skip.note_type, "pitfall");
        assert!(
            skip.reason.contains("Prevention"),
            "skip reason should mention Prevention, got: {}",
            skip.reason
        );
    }

    #[test]
    fn case_missing_reusable_lesson_is_skipped() {
        let note = case_missing_reusable_lesson_fixture();
        let report = extract_qa_pairs(&[note]);

        assert_eq!(report.pairs.len(), 0, "should not produce a QA pair");
        assert_eq!(report.eligible_count, 1, "note is eligible even if skipped");
        assert_eq!(report.skipped.len(), 1, "should skip exactly one note");

        let skip = &report.skipped[0];
        assert_eq!(skip.permalink, "cases/missing-lesson");
        assert_eq!(skip.note_type, "case");
        assert!(
            skip.reason.contains("Reusable lesson"),
            "skip reason should mention Reusable lesson, got: {}",
            skip.reason
        );
    }

    #[test]
    fn non_pitfall_case_notes_are_not_eligible() {
        let adr = CorpusNoteRow {
            permalink: "decisions/some-adr".to_string(),
            title: "Some ADR".to_string(),
            content: "Body".to_string(),
            note_type: "adr".to_string(),
            folder: "decisions".to_string(),
            status: "active".to_string(),
            tags: vec![],
            retrieval_anchor: None,
            timestamps: LifecycleTimestamps {
                created_at: "2026-01-01T00:00:00.000Z".to_string(),
                updated_at: "2026-01-01T00:00:00.000Z".to_string(),
                last_accessed: "2026-01-01T00:00:00.000Z".to_string(),
            },
            confidence: 1.0,
            embedding: None,
            labels: vec![],
            graph_edges: vec![],
            expected_signals: Default::default(),
        };

        let report = extract_qa_pairs(&[adr]);
        assert!(report.pairs.is_empty());
        assert!(report.skipped.is_empty());
        assert_eq!(report.eligible_count, 0);
    }

    #[test]
    fn non_active_status_is_skipped() {
        let mut note = pitfall_note_fixture();
        note.status = "archived".to_string();

        let report = extract_qa_pairs(&[note]);
        assert!(report.pairs.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].reason.contains("archived"));
        assert_eq!(report.eligible_count, 0, "archived notes are not eligible");
    }

    #[test]
    fn extract_qa_pairs_only_returns_only_pairs() {
        let notes = vec![pitfall_note_fixture(), case_note_fixture()];
        let pairs = extract_qa_pairs_only(&notes);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].note_type, "pitfall");
        assert_eq!(pairs[1].note_type, "case");
    }

    #[test]
    fn qa_id_is_stable_across_calls() {
        let note = pitfall_note_fixture();
        let qa1 = &extract_qa_pairs(std::slice::from_ref(&note)).pairs[0];
        let qa2 = &extract_qa_pairs(&[note]).pairs[0];
        assert_eq!(qa1.qa_id, qa2.qa_id, "QA ID must be deterministic");
    }

    #[test]
    fn qa_pair_round_trips_through_serde() {
        let note = pitfall_note_fixture();
        let qa = &extract_qa_pairs(&[note]).pairs[0];

        let json = serde_json::to_string(qa).unwrap();
        let round_tripped: QaPair = serde_json::from_str(&json).unwrap();
        assert_eq!(*qa, round_tripped);
    }

    #[test]
    fn extraction_report_round_trips_through_serde() {
        let notes = vec![pitfall_note_fixture(), case_note_fixture()];
        let report = extract_qa_pairs(&notes);

        let json = serde_json::to_string(&report).unwrap();
        let round_tripped: QaExtractionReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, round_tripped);
    }

    #[test]
    fn mixed_corpus_extracts_and_skips_correctly() {
        let notes = vec![
            pitfall_note_fixture(),               // ✅ extracts
            case_note_fixture(),                  // ✅ extracts
            pitfall_missing_prevention_fixture(), // ❌ skips (empty Prevention body)
        ];

        let report = extract_qa_pairs(&notes);
        assert_eq!(report.pairs.len(), 2);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.eligible_count, 3);
    }

    #[test]
    fn age_days_computation() {
        // Same day → 0
        assert_eq!(age_days("2026-01-01T00:00:00Z", "2026-01-01T23:59:59Z"), 0);
        // Next day → 1
        assert_eq!(age_days("2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"), 1);
        // 30 days
        assert_eq!(age_days("2026-06-01T10:00:00Z", "2026-07-01T10:00:00Z"), 30);
        // Leap year boundary
        assert_eq!(age_days("2026-02-28T00:00:00Z", "2026-03-01T00:00:00Z"), 1);
        // Bad input → 0
        assert_eq!(age_days("garbage", "2026-01-01T00:00:00Z"), 0);
        assert_eq!(age_days("2026-01-01T00:00:00Z", "garbage"), 0);
    }

    #[test]
    fn extract_section_body_returns_none_for_missing_heading() {
        assert!(extract_section_body("## Something\nBody", "Other").is_none());
    }

    #[test]
    fn extract_section_body_returns_none_for_empty_body() {
        assert!(extract_section_body("## Target\n\n## Next", "Target").is_none());
    }

    #[test]
    fn extract_section_body_returns_body_until_next_heading() {
        let content = "## First\nFirst body\n\n## Second\nSecond body\n\n## Third\nThird body";
        assert_eq!(
            extract_section_body(content, "First").as_deref(),
            Some("First body")
        );
        assert_eq!(
            extract_section_body(content, "Second").as_deref(),
            Some("Second body")
        );
        assert_eq!(
            extract_section_body(content, "Third").as_deref(),
            Some("Third body")
        );
    }

    #[test]
    fn empty_corpus_produces_empty_report() {
        let report = extract_qa_pairs(&[]);
        assert!(report.pairs.is_empty());
        assert!(report.skipped.is_empty());
        assert_eq!(report.eligible_count, 0);
    }
}
