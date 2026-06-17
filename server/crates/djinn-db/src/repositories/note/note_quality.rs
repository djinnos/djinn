//! ADR-054 note-quality classifier — the single source of truth for
//! "is this extracted case/pattern/pitfall note too underspecified for durable
//! memory?"
//!
//! Both the extraction admission gate (`djinn-agent`'s `llm_extraction.rs`) and
//! the corpus audit (`graph.rs::extracted_note_audit`) call
//! [`assess_note_quality`] so the two paths cannot drift. The section check is
//! *in-order*: the required headings must appear in their canonical ADR-054
//! sequence, which matches the extraction prompt's durability guarantee.

use djinn_memory::NoteQualityAssessment;

/// Minimum trimmed body length (in chars) for a durable extracted note.
pub(crate) const MIN_DURABLE_BODY_CHARS: usize = 220;
/// Minimum non-empty paragraph count for a durable extracted note.
pub(crate) const MIN_DURABLE_PARAGRAPHS: usize = 3;

/// Canonical ADR-054 required section headings for each gated note type, in the
/// order they must appear in the content body.
///
/// Headings are stored **without** the `## ` prefix so callers can reuse them as
/// plain labels; [`assess_note_quality`] re-adds the prefix when matching
/// against `## {section}` markdown headings.
pub fn required_sections(note_type: &str) -> &'static [&'static str] {
    match note_type {
        "pattern" => &[
            "Context",
            "Problem shape",
            "Recommended approach",
            "Why it works",
            "Tradeoffs / limits",
            "When to use",
            "When not to use",
            "Related",
        ],
        "pitfall" => &[
            "Trigger / smell",
            "Failure mode",
            "Observable symptoms",
            "Prevention",
            "Recovery",
            "Related",
        ],
        "case" => &[
            "Situation",
            "Constraint",
            "Approach taken",
            "Result",
            "Why it worked / failed",
            "Reusable lesson",
            "Related",
        ],
        _ => &[],
    }
}

/// Return the required sections that are absent *or out of canonical order* in
/// `content`. Sections are matched as markdown headings (`## {section}`); a
/// section is reported missing unless every earlier required heading precedes
/// it, so swapping two headings flags the later one as missing.
pub fn missing_required_sections(content: &str, required: &[&str]) -> Vec<String> {
    let mut cursor = 0usize;
    let mut missing = Vec::new();
    for section in required {
        let heading = format!("## {section}");
        match content[cursor..].find(&heading) {
            Some(found_at) => cursor += found_at + heading.len(),
            None => missing.push((*section).to_string()),
        }
    }
    missing
}

/// Heuristic for content that reads as task/session-local working context
/// rather than durable reusable knowledge. Shared by the audit's demote
/// finding; kept adjacent to the classifier so the predicates stay together.
pub fn looks_task_local(title: &str, content: &str) -> bool {
    let haystack = format!("{}\n{}", title.to_lowercase(), content.to_lowercase());
    [
        "this session",
        "current task",
        "working note",
        "working spec",
        "follow-up work",
        "could not be updated from this session",
        "drafted locally",
        "active follow-up work",
        "next session",
    ]
    .iter()
    .any(|needle| haystack.contains(needle))
}

/// Classify a `case`, `pattern`, or `pitfall` note body against the ADR-054
/// quality gate.
///
/// The assessment combines three signals, all derived from `content`:
///
/// 1. **Required sections present and in order** — every heading in
///    [`required_sections`] must appear as a `## {section}` markdown heading in
///    the canonical sequence.
/// 2. **Body length** — the trimmed content must be at least
///    [`MIN_DURABLE_BODY_CHARS`] chars.
/// 3. **Paragraph density** — the content must contain at least
///    [`MIN_DURABLE_PARAGRAPHS`] non-empty `\n\n`-delimited blocks.
///
/// A note is [`NoteQualityAssessment::is_underspecified`] when any signal fails
/// or when `note_type` is not one of the gated types (defensive default — an
/// unknown type has no defined required shape and is treated as underspecified).
pub fn assess_note_quality(note_type: &str, content: &str) -> NoteQualityAssessment {
    let required = required_sections(note_type);
    let missing_sections = missing_required_sections(content, required);
    let content_len = content.trim().chars().count();
    let paragraph_count = content
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .count();

    let mut reasons = Vec::new();
    if required.is_empty() {
        // Unknown note_type: no required-section contract is defined, so the
        // note cannot satisfy the gate. Defensive default keeps un-typed or
        // mistyped content from slipping into durable memory.
        reasons.push(format!("Unknown gated note type '{note_type}'"));
    }
    if !missing_sections.is_empty() {
        reasons.push(format!(
            "Missing ADR-054 required sections: {}",
            missing_sections.join(", ")
        ));
    }
    if content_len < MIN_DURABLE_BODY_CHARS {
        reasons.push(format!(
            "Body is too short for durable memory ({content_len} chars)"
        ));
    }
    if paragraph_count < MIN_DURABLE_PARAGRAPHS {
        reasons.push(format!(
            "Body has only {paragraph_count} paragraph(s); strengthen with explicit context, rationale, and transfer lesson"
        ));
    }

    let is_underspecified = !reasons.is_empty();
    NoteQualityAssessment {
        missing_sections,
        content_len,
        paragraph_count,
        is_underspecified,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_case_sections() -> String {
        [
            "## Situation",
            "## Constraint",
            "## Approach taken",
            "## Result",
            "## Why it worked / failed",
            "## Reusable lesson",
            "## Related",
        ]
        .join("\n\nDurable body paragraph with enough substance to pass the gate. ")
            + "\n\nAdditional context paragraph for density.\n\n"
            + "Closing rationale that explains why the lesson generalizes beyond this task."
    }

    #[test]
    fn case_with_all_required_headings_in_order_is_not_underspecified() {
        let content = all_case_sections();
        let assessment = assess_note_quality("case", &content);
        assert!(
            !assessment.is_underspecified,
            "well-formed case must pass: {:?}",
            assessment.reasons
        );
        assert!(assessment.missing_sections.is_empty());
    }

    #[test]
    fn case_missing_reusable_lesson_is_underspecified() {
        let mut content = all_case_sections();
        content = content.replace("## Reusable lesson\n\n", "");
        let assessment = assess_note_quality("case", &content);
        assert!(assessment.is_underspecified);
        assert!(
            assessment
                .missing_sections
                .contains(&"Reusable lesson".to_string()),
            "missing_sections must flag Reusable lesson: {:?}",
            assessment.missing_sections
        );
    }

    #[test]
    fn pattern_body_too_short_is_underspecified() {
        let content = [
            "## Context",
            "## Problem shape",
            "## Recommended approach",
            "## Why it works",
            "## Tradeoffs / limits",
            "## When to use",
            "## When not to use",
            "## Related",
        ]
        .join("\n\nx");
        let assessment = assess_note_quality("pattern", &content);
        assert!(assessment.is_underspecified);
        assert!(
            assessment.reasons.iter().any(|r| r.contains("too short")),
            "must report a too-short reason: {:?}",
            assessment.reasons
        );
    }

    #[test]
    fn pitfall_with_only_two_paragraphs_is_underspecified() {
        // All pitfall sections present and in order, but the body is collapsed
        // into only two `\n\n`-delimited blocks (headings are separated by a
        // single newline, not a blank line), so the paragraph-density check
        // fires. Body is padded past the 220-char floor to isolate the
        // paragraph signal.
        let content = [
            "## Trigger / smell\n## Failure mode\n## Observable symptoms\n\
             ## Prevention\n## Recovery\n## Related",
            "Combined body that is long enough to clear the 220-character floor \
             for durable memory so only the paragraph-density signal fires here. \
             This pitfall note still only has two paragraphs total which is \
             below the three-paragraph minimum required for a durable note body.",
        ]
        .join("\n\n");
        let assessment = assess_note_quality("pitfall", &content);
        assert!(assessment.is_underspecified);
        assert!(
            assessment.reasons.iter().any(|r| r.contains("paragraph")),
            "must report a low-paragraph reason: {:?}",
            assessment.reasons
        );
    }

    #[test]
    fn out_of_order_headings_are_underspecified() {
        // `## Related` before `## Situation` breaks the canonical order; the
        // in-order scan flags every heading after the first out-of-order one.
        let mut content = String::new();
        let mut sections: Vec<&str> = vec![
            "Situation",
            "Constraint",
            "Approach taken",
            "Result",
            "Why it worked / failed",
            "Reusable lesson",
            "Related",
        ];
        // Move `Related` to the front.
        if let Some(pos) = sections.iter().position(|s| *s == "Related") {
            let related = sections.remove(pos);
            sections.insert(0, related);
        }
        for section in &sections {
            content.push_str(&format!(
                "## {section}\n\nDurable body paragraph with enough substance to pass the gate. "
            ));
        }
        content.push_str(
            "\n\nAdditional context paragraph for density.\n\nClosing rationale that explains why \
             the lesson generalizes beyond this task.",
        );
        let assessment = assess_note_quality("case", &content);
        assert!(
            assessment.is_underspecified,
            "out-of-order headings must be flagged: {:?}",
            assessment.reasons
        );
        assert!(!assessment.missing_sections.is_empty());
    }

    #[test]
    fn unknown_note_type_is_underspecified() {
        let assessment = assess_note_quality("bogus", "any content");
        assert!(assessment.is_underspecified);
        assert!(
            assessment
                .reasons
                .iter()
                .any(|r| r.contains("Unknown gated note type")),
            "must report unknown type reason: {:?}",
            assessment.reasons
        );
    }
}
