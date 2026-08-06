//! Proposal `u46i`: applicability anchors + bounded actionable memory excerpts.
//!
//! Coverage map for the proposal's acceptance criteria:
//!
//! - AC2 field precedence + non-truncated permalink → `anchor_*`, `summary_*`
//! - AC3 extraction grammar                         → `extracts_*`, `section_*`,
//!   `heading_*`, `duplicate_*`, `level_*`, `absent_*`, `unclosed_*`
//! - AC4 byte / truncation contract                 → `action_*`, `marker_*`,
//!   `multibyte_*`, `line_byte_cap_*`, `fenced_block_is_indivisible_*`
//! - AC5 entry atomicity + non-starvation           → `oversized_entry_*`,
//!   `injected_entry_*`, `total_budget_*`
//! - AC6 boundary / legacy / degradation coverage   → the above, collectively
//! - AC7 real-shaped tool-schema fixture            → `tool_schema_fixture_*`

use super::code_context::{extract_action_units, l0_summary, render_action_block};
use super::{
    ACTION_EXCERPT_CAP, ActionExcerptDetail, KnowledgePackConfig, NotePackDisposition,
    NotePackOutcome, pack_ranked_knowledge_notes,
};
use djinn_memory::Note;

// ── Fixtures and helpers ───────────────────────────────────────────────────

fn base_note(permalink: &str, title: &str, content: &str) -> Note {
    Note {
        id: format!("note:{permalink}"),
        project_id: "project_test".to_string(),
        permalink: permalink.to_string(),
        title: title.to_string(),
        file_path: String::new(),
        storage: "db".to_string(),
        note_type: "pitfall".to_string(),
        folder: permalink.split('/').next().unwrap_or("").to_string(),
        status: "active".to_string(),
        tags: "[]".to_string(),
        content: content.to_string(),
        retrieval_anchor: None,
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        updated_at: "2026-01-01T00:00:00.000Z".to_string(),
        lifecycle_changed_at: None,
        last_accessed: "2026-01-01T00:00:00.000Z".to_string(),
        access_count: 0,
        confidence: 0.9,
        abstract_: None,
        overview: None,
        scope_paths: "[]".to_string(),
    }
}

fn anchored_note(permalink: &str, anchor: Option<&str>, content: &str) -> Note {
    let mut note = base_note(permalink, "T", content);
    note.retrieval_anchor = anchor.map(str::to_string);
    note
}

/// Default production knowledge-injection settings
/// (`KnowledgeInjectionConfig::DEFAULT_*`): 8192-byte pack budget, 1024-byte
/// physical-line cap, 10 selected notes. Unchanged by this proposal.
fn default_config() -> KnowledgePackConfig {
    KnowledgePackConfig {
        minimum_confidence: 0.0,
        top_k: 10,
        total_byte_budget: 8192,
        line_byte_cap: 1024,
    }
}

/// The deterministic pull marker for `permalink`.
fn marker_for(permalink: &str) -> String {
    format!("  action: … truncated; memory_read({permalink})")
}

/// Bytes owned by the action allocation of one rendered entry: everything
/// after the summary line's terminating newline.
fn action_allocation_bytes(entry: &str) -> usize {
    match entry.find('\n') {
        Some(index) => entry.len() - index - 1,
        None => 0,
    }
}

/// The action lines of one rendered entry, without the summary line.
fn action_lines(entry: &str) -> Vec<&str> {
    entry.split('\n').skip(1).collect()
}

/// Render exactly one note through the production packer, returning the packed
/// text and that candidate's single outcome.
fn pack_one(note: &Note, config: KnowledgePackConfig) -> (String, NotePackOutcome) {
    let packed = pack_ranked_knowledge_notes(std::slice::from_ref(note), config);
    assert_eq!(
        packed.outcomes.len(),
        1,
        "exactly one outcome per candidate"
    );
    let outcome = packed
        .outcomes
        .first()
        .cloned()
        .unwrap_or_else(|| unreachable!("length asserted above"));
    (packed.rendered, outcome)
}

/// A note whose `## Prevention` body is `lines`, surrounded by realistic
/// neighbouring sections so section boundaries are genuinely exercised.
fn prevention_note(permalink: &str, lines: &[&str]) -> Note {
    let content = format!(
        "## Trigger\n\nSomething went wrong.\n\n## Prevention\n\n{}\n\n## Related\n\n- x\n",
        lines.join("\n")
    );
    anchored_note(permalink, Some("when the anchor applies"), &content)
}

// ── AC2: field precedence ──────────────────────────────────────────────────

#[test]
fn anchor_wins_over_abstract_and_content() {
    let mut note = base_note(
        "pitfalls/anchor-first",
        "Anchor first",
        "The content that must lose.",
    );
    note.abstract_ = Some("The abstract that must lose.".to_string());
    note.retrieval_anchor = Some("Editing MCP tool schemas in this repo.".to_string());

    assert_eq!(l0_summary(&note), "Editing MCP tool schemas in this repo.");
    let (rendered, _) = pack_one(&note, default_config());
    assert!(
        rendered.contains("Editing MCP tool schemas in this repo."),
        "anchor must be the injected summary payload: {rendered}"
    );
    assert!(
        !rendered.contains("The abstract that must lose."),
        "abstract must not win over a non-empty anchor: {rendered}"
    );
    assert!(
        !rendered.contains("The content that must lose."),
        "content must not win over a non-empty anchor: {rendered}"
    );
}

#[test]
fn blank_anchor_falls_back_to_abstract() {
    let mut note = base_note("pitfalls/blank-anchor", "Blank anchor", "Content loses.");
    note.abstract_ = Some("The abstract wins here.".to_string());
    // Whitespace-only values count as empty.
    note.retrieval_anchor = Some("   \t \n  ".to_string());

    assert_eq!(l0_summary(&note), "The abstract wins here.");
    let (rendered, _) = pack_one(&note, default_config());
    assert!(rendered.contains("The abstract wins here."), "{rendered}");
    assert!(!rendered.contains("Content loses."), "{rendered}");
}

#[test]
fn null_anchor_and_blank_abstract_fall_back_to_content() {
    let mut note = base_note(
        "pitfalls/content-fallback",
        "Content fallback",
        "Only the content is left.",
    );
    note.abstract_ = Some("   ".to_string());
    note.retrieval_anchor = None;

    assert_eq!(l0_summary(&note), "Only the content is left.");
    let (rendered, _) = pack_one(&note, default_config());
    assert!(rendered.contains("Only the content is left."), "{rendered}");
}

#[test]
fn all_blank_fields_degrade_to_the_no_abstract_placeholder() {
    let mut note = base_note("pitfalls/empty", "Empty", " ");
    note.abstract_ = Some(" ".to_string());
    note.retrieval_anchor = Some("\n".to_string());
    assert_eq!(l0_summary(&note), "(no abstract)");
}

#[test]
fn summary_truncation_never_removes_the_permalink() {
    let permalink = "pitfalls/very-long-anchor-note";
    let mut note = base_note(permalink, "T", "body");
    note.retrieval_anchor = Some("X".repeat(4000));

    let config = KnowledgePackConfig {
        line_byte_cap: 200,
        ..default_config()
    };
    let (rendered, _) = pack_one(&note, config);
    let summary = rendered.split('\n').next().unwrap_or_default();

    assert!(
        summary.ends_with(&format!(" (permalink: {permalink})")),
        "permalink must survive summary truncation: {summary}"
    );
    assert!(
        summary.contains('…'),
        "an over-long anchor must be truncated with an ellipsis: {summary}"
    );
    assert!(
        summary.len() <= 200,
        "summary must obey line_byte_cap, got {} bytes",
        summary.len()
    );
}

// ── AC3: actionable-section extraction grammar ─────────────────────────────

#[test]
fn extracts_prevention_section_body() {
    let note = prevention_note("pitfalls/basic", &["Do the safe thing."]);
    let (rendered, outcome) = pack_one(&note, default_config());
    assert!(
        rendered.contains("  action: Do the safe thing."),
        "{rendered}"
    );
    assert!(
        !rendered.contains("Something went wrong."),
        "neighbouring sections must not leak into the excerpt: {rendered}"
    );
    assert!(
        !rendered.contains("- x"),
        "the section must end at the next level-2 heading: {rendered}"
    );
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Full));
}

#[test]
fn extracts_recommended_approach_when_prevention_is_absent() {
    let note = anchored_note(
        "patterns/approach",
        Some("anchor"),
        "## Context\n\nc\n\n## Recommended approach\n\nPrefer the cutover.\n",
    );
    let (rendered, _) = pack_one(&note, default_config());
    assert!(
        rendered.contains("  action: Prefer the cutover."),
        "{rendered}"
    );
}

#[test]
fn prevention_takes_precedence_over_recommended_approach() {
    // `Recommended approach` appears FIRST in the document; `Prevention` still
    // wins because precedence is by heading, not by document order.
    let note = anchored_note(
        "pitfalls/precedence",
        Some("anchor"),
        "## Recommended approach\n\nApproach line.\n\n## Prevention\n\nPrevention line.\n",
    );
    let (rendered, _) = pack_one(&note, default_config());
    assert!(rendered.contains("Prevention line."), "{rendered}");
    assert!(!rendered.contains("Approach line."), "{rendered}");
}

#[test]
fn heading_match_is_ascii_case_folded_and_exact() {
    let upper = anchored_note(
        "pitfalls/upper",
        Some("anchor"),
        "## PREVENTION ##\n\nUpper-case heading body.\n",
    );
    let (rendered, _) = pack_one(&upper, default_config());
    assert!(rendered.contains("Upper-case heading body."), "{rendered}");

    let mixed = anchored_note(
        "pitfalls/mixed",
        Some("anchor"),
        "## rEcOmMeNdEd ApPrOaCh\n\nMixed-case heading body.\n",
    );
    let (rendered, _) = pack_one(&mixed, default_config());
    assert!(rendered.contains("Mixed-case heading body."), "{rendered}");

    // Near-miss headings are not eligible — the match is exact.
    let near_miss = anchored_note(
        "pitfalls/near-miss",
        Some("anchor"),
        "## Prevention notes\n\nNot eligible.\n\n## Recommended approaches\n\nAlso not.\n",
    );
    let (rendered, outcome) = pack_one(&near_miss, default_config());
    assert!(!rendered.contains("Not eligible."), "{rendered}");
    assert!(!rendered.contains("Also not."), "{rendered}");
    assert_eq!(outcome.action_excerpt, None);
}

#[test]
fn only_level_two_headings_are_eligible() {
    let note = anchored_note(
        "pitfalls/levels",
        Some("anchor"),
        "# Prevention\n\nLevel one body.\n\n### Prevention\n\nLevel three body.\n",
    );
    let (rendered, outcome) = pack_one(&note, default_config());
    assert!(!rendered.contains("Level one body."), "{rendered}");
    assert!(!rendered.contains("Level three body."), "{rendered}");
    assert_eq!(outcome.action_excerpt, None);
}

#[test]
fn duplicate_headings_take_the_first_non_empty_section() {
    let note = anchored_note(
        "pitfalls/dupes",
        Some("anchor"),
        "## Prevention\n\n## Prevention\n\nSecond section body.\n\n## Prevention\n\nThird.\n",
    );
    let (rendered, _) = pack_one(&note, default_config());
    assert!(rendered.contains("Second section body."), "{rendered}");
    assert!(!rendered.contains("Third."), "{rendered}");
}

#[test]
fn empty_prevention_falls_back_to_recommended_approach() {
    let note = anchored_note(
        "pitfalls/empty-prevention",
        Some("anchor"),
        "## Prevention\n\n   \n\n## Recommended approach\n\nFallback body.\n",
    );
    let (rendered, _) = pack_one(&note, default_config());
    assert!(rendered.contains("  action: Fallback body."), "{rendered}");
}

#[test]
fn absent_sections_render_a_summary_only_entry() {
    let note = anchored_note(
        "pitfalls/no-section",
        Some("still applies here"),
        "Legacy free-form body with no headings whatsoever.\n",
    );
    let (rendered, outcome) = pack_one(&note, default_config());
    assert_eq!(
        rendered.split('\n').count(),
        1,
        "summary-only entry expected: {rendered}"
    );
    assert!(rendered.contains("still applies here"), "{rendered}");
    assert_eq!(
        outcome.disposition,
        NotePackDisposition::Injected,
        "a candidate is never dropped merely for lacking an action section"
    );
    assert_eq!(outcome.action_excerpt, None);
}

#[test]
fn level_three_headings_belong_to_the_section() {
    let note = anchored_note(
        "pitfalls/nested",
        Some("anchor"),
        "## Prevention\n\nTop line.\n\n### Details\n\nNested line.\n\n## Related\n\n- y\n",
    );
    let (rendered, _) = pack_one(&note, default_config());
    assert!(rendered.contains("Top line."), "{rendered}");
    assert!(rendered.contains("### Details"), "{rendered}");
    assert!(rendered.contains("Nested line."), "{rendered}");
    assert!(!rendered.contains("- y"), "{rendered}");
}

#[test]
fn section_ends_at_the_next_level_one_heading() {
    let note = anchored_note(
        "pitfalls/level-one-end",
        Some("anchor"),
        "## Prevention\n\nKept line.\n\n# Appendix\n\nDropped line.\n",
    );
    let (rendered, _) = pack_one(&note, default_config());
    assert!(rendered.contains("Kept line."), "{rendered}");
    assert!(!rendered.contains("Dropped line."), "{rendered}");
}

#[test]
fn heading_like_text_in_quotes_and_fenced_code_is_not_a_heading() {
    let content = concat!(
        "## Summary\n\n",
        "Nothing actionable here.\n\n",
        "> ## Prevention\n",
        "> Quoted, not a heading.\n\n",
        "```\n",
        "## Prevention\n",
        "Fenced, not a heading.\n",
        "```\n",
    );
    let note = anchored_note("pitfalls/fake-headings", Some("anchor"), content);
    let (rendered, outcome) = pack_one(&note, default_config());
    assert_eq!(
        rendered.split('\n').count(),
        1,
        "no excerpt may be produced from fake headings: {rendered}"
    );
    assert!(!rendered.contains("Quoted, not a heading."), "{rendered}");
    assert!(!rendered.contains("Fenced, not a heading."), "{rendered}");
    assert_eq!(outcome.action_excerpt, None);
}

#[test]
fn fenced_block_inside_a_section_is_preserved_whole() {
    let content = concat!(
        "## Prevention\n\n",
        "Run this:\n\n",
        "```sh\n",
        "## not a heading inside a fence\n",
        "cargo test -p djinn-slot\n",
        "```\n\n",
        "Then verify.\n\n",
        "## Related\n\n- z\n",
    );
    let note = anchored_note("pitfalls/fenced", Some("anchor"), content);
    let (rendered, outcome) = pack_one(&note, default_config());
    assert!(rendered.contains("```sh"), "{rendered}");
    assert!(
        rendered.contains("## not a heading inside a fence"),
        "a fenced heading must not terminate the section: {rendered}"
    );
    assert!(rendered.contains("cargo test -p djinn-slot"), "{rendered}");
    assert!(rendered.contains("Then verify."), "{rendered}");
    assert!(!rendered.contains("- z"), "{rendered}");
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Full));
}

#[test]
fn unclosed_fence_is_malformed_and_yields_no_excerpt() {
    let content = concat!(
        "## Prevention\n\n",
        "Run this:\n\n",
        "```sh\n",
        "cargo test -p djinn-slot\n",
    );
    let note = anchored_note("pitfalls/unclosed", Some("anchor"), content);
    assert!(
        extract_action_units(&note.content).is_none(),
        "an unclosed fence must be treated as malformed"
    );
    let (rendered, outcome) = pack_one(&note, default_config());
    assert_eq!(
        rendered.split('\n').count(),
        1,
        "malformed sections degrade to summary-only: {rendered}"
    );
    assert_eq!(outcome.disposition, NotePackDisposition::Injected);
    assert_eq!(outcome.action_excerpt, None);
}

// ── AC4: byte and truncation contract ──────────────────────────────────────

#[test]
fn action_allocation_of_exactly_640_bytes_renders_without_a_marker() {
    // 10-byte prefix + 630-byte body = exactly ACTION_EXCERPT_CAP.
    let body = "A".repeat(ACTION_EXCERPT_CAP - "  action: ".len());
    let note = prevention_note("pitfalls/exact", &[&body]);
    let (rendered, outcome) = pack_one(&note, default_config());

    assert_eq!(
        action_allocation_bytes(&rendered),
        ACTION_EXCERPT_CAP,
        "the action block must land exactly on the cap"
    );
    assert!(!rendered.contains("truncated"), "{rendered}");
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Full));
}

#[test]
fn action_allocation_one_byte_over_640_drops_the_unit_for_the_marker() {
    let body = "A".repeat(ACTION_EXCERPT_CAP - "  action: ".len() + 1);
    let permalink = "pitfalls/one-over";
    let note = prevention_note(permalink, &[&body]);
    let (rendered, outcome) = pack_one(&note, default_config());

    assert!(
        action_allocation_bytes(&rendered) <= ACTION_EXCERPT_CAP,
        "the action block must never exceed the cap"
    );
    assert!(
        !rendered.contains("AAAA"),
        "an over-cap source line must never be byte-sliced: {rendered}"
    );
    assert_eq!(action_lines(&rendered), vec![marker_for(permalink)]);
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Omitted));
}

#[test]
fn multi_line_action_block_fits_exactly_and_one_byte_over_truncates() {
    let permalink = "pitfalls/multi";
    let first = "A".repeat(300);
    // 310 + 1 (newline) + 10 (prefix) + 319 = 640 exactly.
    let second_exact = "B".repeat(319);
    let note = prevention_note(permalink, &[&first, &second_exact]);
    let (rendered, outcome) = pack_one(&note, default_config());
    assert_eq!(action_allocation_bytes(&rendered), ACTION_EXCERPT_CAP);
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Full));
    assert!(
        rendered.contains(&format!("          {second_exact}")),
        "{rendered}"
    );

    // One byte more and the second line no longer fits; the marker replaces it.
    let second_over = "B".repeat(320);
    let note = prevention_note(permalink, &[&first, &second_over]);
    let (rendered, outcome) = pack_one(&note, default_config());
    assert!(action_allocation_bytes(&rendered) <= ACTION_EXCERPT_CAP);
    assert!(
        !rendered.contains('B'),
        "the over-cap line must be dropped whole, not sliced: {rendered}"
    );
    assert_eq!(
        action_lines(&rendered),
        vec![format!("  action: {first}"), marker_for(permalink)]
    );
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Truncated));
}

#[test]
fn multibyte_boundary_is_never_split() {
    let permalink = "pitfalls/multibyte";
    // 210 × 3-byte scalars = 630 bytes → exactly on the cap with the prefix.
    let exact = "日".repeat(210);
    let note = prevention_note(permalink, &[&exact]);
    let (rendered, outcome) = pack_one(&note, default_config());
    assert_eq!(action_allocation_bytes(&rendered), ACTION_EXCERPT_CAP);
    assert_eq!(rendered.matches('日').count(), 210, "{rendered}");
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Full));

    // One scalar more crosses the cap mid-character if it were byte-sliced.
    let over = "日".repeat(211);
    let note = prevention_note(permalink, &[&over]);
    let (rendered, _) = pack_one(&note, default_config());
    assert!(action_allocation_bytes(&rendered) <= ACTION_EXCERPT_CAP);
    assert_eq!(
        rendered.matches('日').count(),
        0,
        "no partial multibyte content may survive: {rendered}"
    );
    assert_eq!(action_lines(&rendered), vec![marker_for(permalink)]);
}

#[test]
fn marker_evicts_already_included_units_when_it_does_not_fit() {
    let permalink = "pitfalls/evict";
    let first = "A".repeat(300); // 310 bytes
    let second = "B".repeat(310); // + 1 + 320 = 631 bytes
    let third = "C".repeat(600); // does not fit → truncation begins
    let note = prevention_note(permalink, &[&first, &second, &third]);
    let (rendered, outcome) = pack_one(&note, default_config());

    // 631 + 1 + marker does not fit, so the second unit is evicted.
    assert_eq!(
        action_lines(&rendered),
        vec![format!("  action: {first}"), marker_for(permalink)]
    );
    assert!(action_allocation_bytes(&rendered) <= ACTION_EXCERPT_CAP);
    assert!(!rendered.contains('B'), "{rendered}");
    assert!(!rendered.contains('C'), "{rendered}");
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Truncated));
}

#[test]
fn fenced_block_is_indivisible_and_is_replaced_wholesale_by_the_marker() {
    let permalink = "pitfalls/fence-cap";
    let long_command = "cargo test ".repeat(60); // ~660 bytes
    let content =
        format!("## Prevention\n\nRun:\n\n```sh\n{long_command}\n```\n\n## Related\n\n- q\n");
    let note = anchored_note(permalink, Some("anchor"), &content);
    let (rendered, outcome) = pack_one(&note, default_config());

    assert!(action_allocation_bytes(&rendered) <= ACTION_EXCERPT_CAP);
    assert!(
        !rendered.contains("```"),
        "a fenced block that cannot fit whole must not be partially emitted: {rendered}"
    );
    assert_eq!(
        action_lines(&rendered),
        vec!["  action: Run:".to_string(), marker_for(permalink)]
    );
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Truncated));
}

#[test]
fn line_byte_cap_below_640_bounds_every_physical_line() {
    let permalink = "pitfalls/small-cap";
    let fits = "F".repeat(150); // 160 bytes, under the 200-byte cap
    let too_long = "G".repeat(300); // 310 bytes, over the cap
    let note = prevention_note(permalink, &[&fits, &too_long]);
    let config = KnowledgePackConfig {
        line_byte_cap: 200,
        ..default_config()
    };
    let (rendered, outcome) = pack_one(&note, config);

    for line in rendered.split('\n') {
        assert!(
            line.len() <= 200,
            "physical line exceeds line_byte_cap ({} bytes): {line}",
            line.len()
        );
    }
    assert!(
        rendered.contains(&format!("  action: {fits}")),
        "{rendered}"
    );
    assert!(
        !rendered.contains('G'),
        "a source line over line_byte_cap must never be sliced: {rendered}"
    );
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Truncated));
    assert!(action_allocation_bytes(&rendered) <= ACTION_EXCERPT_CAP);
}

#[test]
fn marker_that_cannot_obey_line_byte_cap_yields_no_action_block() {
    // Spec step 6, last clause. Only reachable at the renderer boundary: for
    // the same permalink the marker is always shorter than the shortest
    // renderable summary line, so `pack_ranked_knowledge_notes` reports
    // `OversizedSkipped` before this can be hit end-to-end.
    let permalink = "pitfalls/".to_string() + &"p".repeat(120);
    let content = format!("## Prevention\n\n{}\n", "H".repeat(900));
    let units = extract_action_units(&content).unwrap_or_default();
    assert!(!units.is_empty(), "the section must parse");

    let marker_len = marker_for(&permalink).len();
    assert!(
        render_action_block(&units, &permalink, marker_len - 1).is_none(),
        "no action block may be rendered when even the marker breaks the cap"
    );
    // One byte more of headroom and the marker alone is rendered.
    match render_action_block(&units, &permalink, marker_len) {
        Some(block) => {
            assert_eq!(block.lines, vec![marker_for(&permalink)]);
            assert_eq!(block.detail, ActionExcerptDetail::Omitted);
        }
        None => panic!("the marker alone must fit at exactly its own length"),
    }
}

#[test]
fn total_budget_exact_fit_injects_and_one_byte_less_prunes() {
    let note = prevention_note("pitfalls/budget", &["Short but real guidance."]);
    let (rendered, _) = pack_one(&note, default_config());
    let entry_bytes = rendered.len();
    assert!(entry_bytes > 0);

    let exact = KnowledgePackConfig {
        total_byte_budget: entry_bytes,
        ..default_config()
    };
    let (rendered_exact, outcome_exact) = pack_one(&note, exact);
    assert_eq!(outcome_exact.disposition, NotePackDisposition::Injected);
    assert_eq!(rendered_exact.len(), entry_bytes);
    assert_eq!(outcome_exact.estimated_rendered_chars, Some(entry_bytes));

    let one_short = KnowledgePackConfig {
        total_byte_budget: entry_bytes - 1,
        ..default_config()
    };
    let (rendered_short, outcome_short) = pack_one(&note, one_short);
    assert_eq!(outcome_short.disposition, NotePackDisposition::BudgetPruned);
    assert!(
        rendered_short.is_empty(),
        "no partial entry may be emitted: {rendered_short}"
    );
}

// ── AC5: entry atomicity and non-starvation ────────────────────────────────

#[test]
fn oversized_entry_is_wholly_pruned_and_a_later_smaller_entry_still_injects() {
    let big = prevention_note("pitfalls/big", &["B".repeat(400).as_str()]);
    let small = anchored_note(
        "pitfalls/small",
        Some("small anchor"),
        "Legacy body, no sections.\n",
    );

    let (small_rendered, _) = pack_one(&small, default_config());
    let (big_rendered, _) = pack_one(&big, default_config());
    // A budget that fits the small entry but not the big one.
    let budget = big_rendered.len() - 1;
    assert!(budget >= small_rendered.len());

    let notes = vec![big, small];
    let packed = pack_ranked_knowledge_notes(
        &notes,
        KnowledgePackConfig {
            total_byte_budget: budget,
            ..default_config()
        },
    );

    assert_eq!(
        packed.outcomes.len(),
        2,
        "exactly one outcome per candidate"
    );
    assert_eq!(
        packed.outcomes[0].disposition,
        NotePackDisposition::BudgetPruned
    );
    assert_eq!(
        packed.outcomes[1].disposition,
        NotePackDisposition::Injected
    );
    assert!(
        !packed.rendered.contains("pitfalls/big"),
        "no line of a pruned entry may appear: {}",
        packed.rendered
    );
    assert!(
        !packed.rendered.contains('B'),
        "no action line of a pruned entry may appear: {}",
        packed.rendered
    );
    assert_eq!(packed.rendered, small_rendered);
    assert_eq!(packed.total_injected_chars, small_rendered.len());
}

#[test]
fn injected_entry_is_atomic_and_carries_one_disposition_plus_action_detail() {
    let note = prevention_note(
        "pitfalls/atomic",
        &["Guidance line one.", "Guidance line two."],
    );
    let (rendered, outcome) = pack_one(&note, default_config());

    let lines: Vec<&str> = rendered.split('\n').collect();
    assert_eq!(lines.len(), 3, "summary + two action lines: {rendered}");
    assert!(lines[0].starts_with("- **[Pitfall] T**: "));
    assert_eq!(lines[1], "  action: Guidance line one.");
    assert_eq!(lines[2], "          Guidance line two.");

    // One disposition; `action_excerpt` is trace detail, not a second one.
    assert_eq!(outcome.disposition, NotePackDisposition::Injected);
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Full));
    assert_eq!(outcome.estimated_rendered_chars, Some(rendered.len()));
}

// ── AC7: real-shaped tool-schema regression fixture ────────────────────────

/// A real-shaped rendition of
/// `pitfalls/tool-schema-edits-must-regenerate-all-derived-goldens-...`: the
/// production note's actual title, permalink, and `retrieval_anchor`, with its
/// regeneration commands authored under a `## Prevention` heading.
pub(super) fn tool_schema_fixture() -> Note {
    let content = concat!(
        "Changing MCP tool schema text fans out into MULTIPLE derived golden files, and\n",
        "regenerating only some of them leaves `main` in a state where the full\n",
        "merge-queue suite fails for EVERY subsequent PR.\n",
        "\n",
        "## Observable symptoms\n",
        "\n",
        "The merge queue fails on schema snapshot / golden mismatches for PRs that never\n",
        "touched a tool schema.\n",
        "\n",
        "## Prevention\n",
        "\n",
        "Regenerate every derived golden in the same commit:\n",
        "\n",
        "- Server insta snap: `INSTA_UPDATE=always cargo test --all-features tool_schemas`\n",
        "- Corpus fixture: `UPDATE_DJINN_MCP_SERVER_FIXTURE=1 cargo test -p djinn-control-plane`\n",
        "- UI types: `pnpm mcp:types:snapshot` (reads the insta snap, so do that one FIRST)\n",
        "\n",
        "## Related\n",
        "\n",
        "- pitfalls/environment-config-schema-golden-fanout\n",
    );
    let mut note = base_note(
        "pitfalls/tool-schema-edits-must-regenerate-all-derived-goldens-or-the-merge-queue-breaks-for-everyone",
        "Tool-schema edits must regenerate ALL derived goldens or the merge queue breaks for everyone",
        content,
    );
    note.retrieval_anchor = Some(TOOL_SCHEMA_ANCHOR.to_string());
    note
}

pub(super) const TOOL_SCHEMA_ANCHOR: &str = "Editing MCP tool schemas/param descriptions in the djinn repo, or the merge-queue suite fails on schema snapshot/golden mismatches.";

#[test]
fn tool_schema_fixture_renders_anchor_and_all_three_regeneration_commands() {
    let note = tool_schema_fixture();
    let config = default_config();
    let (rendered, outcome) = pack_one(&note, config);

    // The applicability anchor is the injected summary payload.
    assert!(
        rendered.contains(TOOL_SCHEMA_ANCHOR),
        "anchor missing from the packed prompt: {rendered}"
    );
    assert!(
        rendered.contains(
            "(permalink: pitfalls/tool-schema-edits-must-regenerate-all-derived-goldens-or-the-merge-queue-breaks-for-everyone)"
        ),
        "permalink missing: {rendered}"
    );

    // All three regeneration commands appear as COMPLETE physical lines.
    let lines: Vec<&str> = rendered.split('\n').collect();
    assert_eq!(lines.len(), 6, "unexpected entry shape: {rendered}");
    assert_eq!(
        lines[1],
        "  action: Regenerate every derived golden in the same commit:"
    );
    assert_eq!(lines[2], "");
    assert_eq!(
        lines[3],
        "          - Server insta snap: `INSTA_UPDATE=always cargo test --all-features tool_schemas`"
    );
    assert_eq!(
        lines[4],
        "          - Corpus fixture: `UPDATE_DJINN_MCP_SERVER_FIXTURE=1 cargo test -p djinn-control-plane`"
    );
    assert_eq!(
        lines[5],
        "          - UI types: `pnpm mcp:types:snapshot` (reads the insta snap, so do that one FIRST)"
    );
    assert!(rendered.contains("INSTA_UPDATE=always"), "{rendered}");
    assert!(
        rendered.contains("UPDATE_DJINN_MCP_SERVER_FIXTURE=1"),
        "{rendered}"
    );
    assert!(rendered.contains("pnpm mcp:types:snapshot"), "{rendered}");

    assert!(!rendered.contains("truncated"), "{rendered}");
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Full));
    assert_eq!(outcome.disposition, NotePackDisposition::Injected);

    // The byte contract still holds under default settings.
    assert!(action_allocation_bytes(&rendered) <= ACTION_EXCERPT_CAP);
    for line in &lines {
        assert!(line.len() <= config.line_byte_cap, "line too long: {line}");
    }
    assert!(rendered.len() <= config.total_byte_budget);
}
