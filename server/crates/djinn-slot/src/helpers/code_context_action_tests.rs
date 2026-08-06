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
    NotePackOutcome, legacy_rendered_line_overhead_bytes, pack_ranked_knowledge_notes,
    rendered_line_overhead_bytes,
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

// ── AC1 (R1): the permalink no longer duplicates the title ─────────────────

/// Twenty real `(note_type, title, permalink)` triples sampled from the live
/// djinn corpus (every 700th pitfall, every 500th pattern, every 500th case),
/// so the overhead measurement is taken against real strings rather than
/// invented ones. They show the duplication R1 removes: the permalink is the
/// slugified title, verbatim.
const REAL_CORPUS_SAMPLE: &[(&str, &str, &str)] = &[
    (
        "pitfall",
        "A bounded catalog without explicit per-section limits is not bounded",
        "pitfalls/a-bounded-catalog-without-explicit-per-section-limits-is-not-bounded",
    ),
    (
        "pitfall",
        "Broad-scope child uniqueness can silently become parent transaction failure",
        "pitfalls/broad-scope-child-uniqueness-can-silently-become-parent-transaction-failure",
    ),
    (
        "pitfall",
        "Do not hand-roll the empty environment-config JSON shape",
        "pitfalls/do-not-hand-roll-the-empty-environment-config-json-shape",
    ),
    (
        "pitfall",
        "Fixing only the first failing fixture causes serial CI rediscovery",
        "pitfalls/fixing-only-the-first-failing-fixture-causes-serial-ci-rediscovery",
    ),
    (
        "pitfall",
        "Narrow-integer arithmetic inside malformed-row validation",
        "pitfalls/narrow-integer-arithmetic-inside-malformed-row-validation",
    ),
    (
        "pitfall",
        "Resuming stale force-closed branches can reintroduce failed hardening work",
        "pitfalls/resuming-stale-force-closed-branches-can-reintroduce-failed-hardening-work",
    ),
    (
        "pitfall",
        "Treating a failed broad Cargo test as a code failure without isolating diagnostics",
        "pitfalls/treating-a-failed-broad-cargo-test-as-a-code-failure-without-isolating-diagnostics",
    ),
    (
        "pitfall",
        "Using an empty JSONB array as proof of an empty historical retrieval",
        "pitfalls/using-an-empty-jsonb-array-as-proof-of-an-empty-historical-retrieval",
    ),
    (
        "pattern",
        "Add backwards-compatible fields to Rust bridge DTOs with serde defaults",
        "patterns/add-backwards-compatible-fields-to-rust-bridge-dtos-with-serde-defaults",
    ),
    (
        "pattern",
        "Deterministic lease-race tests with injected ordering seams",
        "patterns/deterministic-lease-race-tests-with-injected-ordering-seams",
    ),
    (
        "pattern",
        "Keep liveness policy pure and adapt persisted states at the repository boundary",
        "patterns/keep-liveness-policy-pure-and-adapt-persisted-states-at-the-repository-boundary",
    ),
    (
        "pattern",
        "Read-only doctor checks as snapshot source plus pure evaluator",
        "patterns/read-only-doctor-checks-as-snapshot-source-plus-pure-evaluator",
    ),
    (
        "pattern",
        "Stage-discriminating assertions for fallback pipelines",
        "patterns/stage-discriminating-assertions-for-fallback-pipelines",
    ),
    (
        "pattern",
        "Use one canonical turn assembler for normal and interrupted paths",
        "patterns/use-one-canonical-turn-assembler-for-normal-and-interrupted-paths",
    ),
    (
        "case",
        "6eaw tighten agent-worker checkpoint test git allowlist entry",
        "cases/6eaw-tighten-agent-worker-checkpoint-test-git-allowlist-entry",
    ),
    (
        "case",
        "Converging synthetic extraction tool-use payloads with mandatory memory reasons",
        "cases/converging-synthetic-extraction-tool-use-payloads-with-mandatory-memory-reasons",
    ),
    (
        "case",
        "Focused regression module for embedding_related edge-kind filtering",
        "cases/focused-regression-module-for-embedding-related-edge-kind-filtering",
    ),
    (
        "case",
        "Postgres readiness aggregation race fixture",
        "cases/postgres-readiness-aggregation-race-fixture",
    ),
    (
        "case",
        "Repair a thiserror String field compile regression without disturbing generation cutover work",
        "cases/repair-a-thiserror-string-field-compile-regression-without-disturbing-generation-cutover-work",
    ),
    (
        "case",
        "Split resize reconciliation summaries by durable ledger",
        "cases/split-resize-reconciliation-summaries-by-durable-ledger",
    ),
];

fn real_corpus_notes() -> Vec<Note> {
    REAL_CORPUS_SAMPLE
        .iter()
        .map(|(note_type, title, permalink)| {
            let mut note = base_note(permalink, title, "body");
            note.note_type = (*note_type).to_string();
            note.retrieval_anchor = Some("applies when the sampled condition holds".to_string());
            note
        })
        .collect()
}

#[test]
fn permalink_no_longer_duplicates_the_title_and_overhead_roughly_halves() {
    let notes = real_corpus_notes();

    let legacy_total: usize = notes.iter().map(legacy_rendered_line_overhead_bytes).sum();
    let new_total: usize = notes.iter().map(rendered_line_overhead_bytes).sum();
    let reduction = 1.0 - (new_total as f64 / legacy_total as f64);

    assert!(
        reduction >= 0.45,
        "R1 must roughly halve per-line overhead; measured {:.1}% \
         (legacy {legacy_total} B over {} notes, now {new_total} B)",
        reduction * 100.0,
        notes.len()
    );

    // The title is genuinely gone from the rendered line, and the permalink —
    // the pull handle — is genuinely still there.
    for note in &notes {
        let (rendered, _) = pack_one(note, default_config());
        let summary = rendered.split('\n').next().unwrap_or_default();
        assert!(
            summary.contains(&note.permalink),
            "the pull handle must remain on the line: {summary}"
        );
        assert!(
            !summary.contains(&note.title),
            "the title must not be rendered alongside its own slug: {summary}"
        );
        assert_eq!(
            rendered_line_overhead_bytes(note),
            summary.len() - l0_summary(note).len(),
            "measured overhead must match the reported overhead for {}",
            note.permalink
        );
    }
}

#[test]
fn dropping_the_duplicate_title_lets_more_notes_survive_the_line_cap() {
    // R1's operational point: `rendered_line` DROPS a note whose fixed
    // overhead exceeds the cap. Halving overhead moves that cliff, so notes
    // that were silently deleted now render.
    let notes = real_corpus_notes();
    let cap = 160;

    let legacy_survivors = notes
        .iter()
        .filter(|note| legacy_rendered_line_overhead_bytes(note) + "(no abstract)".len() <= cap)
        .count();
    let new_survivors = notes
        .iter()
        .filter(|note| rendered_line_overhead_bytes(note) + "(no abstract)".len() <= cap)
        .count();

    assert!(
        new_survivors > legacy_survivors,
        "R1 must move the drop cliff: {legacy_survivors} → {new_survivors} survivors at cap {cap}"
    );

    let packed = pack_ranked_knowledge_notes(
        &notes,
        KnowledgePackConfig {
            top_k: notes.len(),
            line_byte_cap: cap,
            ..default_config()
        },
    );
    assert_eq!(
        packed
            .outcomes
            .iter()
            .filter(|outcome| outcome.disposition == NotePackDisposition::Injected)
            .count(),
        new_survivors,
        "every note that fits the cap must actually be injected"
    );
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

    // R1: the permalink is the line's label, so it precedes the summary and is
    // structurally impossible to truncate away.
    assert!(
        summary.starts_with(&format!("- **[Pitfall] {permalink}**: ")),
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

/// `case` notes are 2814 of the 10687 notes in the three injected types and
/// their template never uses `Prevention` or `Recommended approach` — the
/// actionable slot is `## Reusable lesson`. Measured on a 120-note spread
/// sample: 0/40 case notes carry either proposal-named heading, 38/40 carry
/// `Reusable lesson`. Without this heading the whole `case` type is dark.
#[test]
fn case_notes_extract_their_reusable_lesson_section() {
    // Shaped on the real case template: Situation / Constraint / Approach
    // taken / Result / Why it worked / failed / Reusable lesson / Related.
    let content = concat!(
        "## Situation\n\nA retrieval query returned stale notes.\n\n",
        "## Constraint\n\nThe scoring contract could not change for old notes.\n\n",
        "## Approach taken\n\nIntroduced a tiered decay curve.\n\n",
        "## Result\n\nRecent notes now rank in the top 5.\n\n",
        "## Why it worked / failed\n\nThe tail behaviour was preserved.\n\n",
        "## Reusable lesson\n\n",
        "When adjusting decay functions, always tier the curve so the long tail is preserved.\n",
        "Test against the >90 day cohort as a regression guard.\n\n",
        "## Related\n\n- decisions/decay-rate-adrs\n",
    );
    let note = base_note("cases/decay-rate-adjustment", "Decay case", content);
    let mut note = note;
    note.note_type = "case".to_string();
    note.retrieval_anchor = Some("adjusting a memory decay function".to_string());

    let (rendered, outcome) = pack_one(&note, default_config());
    assert!(
        rendered.contains(
            "  action: When adjusting decay functions, always tier the curve so the long tail is preserved."
        ),
        "{rendered}"
    );
    assert!(
        rendered.contains("          Test against the >90 day cohort as a regression guard."),
        "{rendered}"
    );
    assert!(
        !rendered.contains("A retrieval query returned stale notes."),
        "only the actionable section may be excerpted: {rendered}"
    );
    assert!(
        !rendered.contains("decisions/decay-rate-adrs"),
        "the section must end at the next level-2 heading: {rendered}"
    );
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Full));
}

/// Precedence across all three eligible headings, independent of the order
/// they appear in the document.
#[test]
fn prevention_beats_recommended_approach_beats_reusable_lesson() {
    let all_three = anchored_note(
        "pitfalls/all-three",
        Some("anchor"),
        "## Reusable lesson\n\nLesson body.\n\n\
         ## Recommended approach\n\nApproach body.\n\n\
         ## Prevention\n\nPrevention body.\n",
    );
    let (rendered, _) = pack_one(&all_three, default_config());
    assert!(rendered.contains("Prevention body."), "{rendered}");
    assert!(!rendered.contains("Approach body."), "{rendered}");
    assert!(!rendered.contains("Lesson body."), "{rendered}");

    let two = anchored_note(
        "patterns/two",
        Some("anchor"),
        "## Reusable lesson\n\nLesson body.\n\n## Recommended approach\n\nApproach body.\n",
    );
    let (rendered, _) = pack_one(&two, default_config());
    assert!(rendered.contains("Approach body."), "{rendered}");
    assert!(!rendered.contains("Lesson body."), "{rendered}");

    let lesson_only = anchored_note(
        "cases/one",
        Some("anchor"),
        "## Situation\n\ns\n\n## Reusable lesson\n\nLesson body.\n",
    );
    let (rendered, _) = pack_one(&lesson_only, default_config());
    assert!(rendered.contains("Lesson body."), "{rendered}");
}

/// An empty `Prevention` must fall through the whole precedence chain, not
/// stop at the first eligible heading it finds.
#[test]
fn empty_sections_fall_through_the_entire_precedence_chain() {
    let note = anchored_note(
        "pitfalls/chain",
        Some("anchor"),
        "## Prevention\n\n   \n\n## Recommended approach\n\n\n\n## Reusable lesson\n\nLast resort.\n",
    );
    let (rendered, _) = pack_one(&note, default_config());
    assert!(rendered.contains("  action: Last resort."), "{rendered}");
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
    // 310 + 1 (newline) + 10 (prefix) + 703 = 1024 exactly.
    let second_exact = "B".repeat(ACTION_EXCERPT_CAP - 310 - 1 - "  action: ".len());
    let note = prevention_note(permalink, &[&first, &second_exact]);
    let (rendered, outcome) = pack_one(&note, default_config());
    assert_eq!(action_allocation_bytes(&rendered), ACTION_EXCERPT_CAP);
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Full));
    assert!(
        rendered.contains(&format!("          {second_exact}")),
        "{rendered}"
    );

    // One byte more and the second line no longer fits; the marker replaces it.
    let second_over = "B".repeat(ACTION_EXCERPT_CAP - 310 - 1 - "  action: ".len() + 1);
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
    // 338 × 3-byte scalars = 1014 bytes → exactly on the cap with the prefix.
    let exact = "日".repeat((ACTION_EXCERPT_CAP - "  action: ".len()) / 3);
    let note = prevention_note(permalink, &[&exact]);
    let (rendered, outcome) = pack_one(&note, default_config());
    assert_eq!(action_allocation_bytes(&rendered), ACTION_EXCERPT_CAP);
    let scalars = (ACTION_EXCERPT_CAP - "  action: ".len()) / 3;
    assert_eq!(rendered.matches('日').count(), scalars, "{rendered}");
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Full));

    // One scalar more crosses the cap mid-character if it were byte-sliced.
    let over = "日".repeat(scalars + 1);
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
    let second = "B".repeat(690); // + 1 + 700 = 1011 bytes, just under the cap
    let third = "C".repeat(600); // does not fit → truncation begins
    let note = prevention_note(permalink, &[&first, &second, &third]);
    let (rendered, outcome) = pack_one(&note, default_config());

    // 1011 + 1 + marker does not fit, so the second unit is evicted.
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
    let long_command = "cargo test ".repeat(120); // 1320 bytes, over the cap
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
    assert!(lines[0].starts_with("- **[Pitfall] pitfalls/atomic**: "));
    assert_eq!(lines[1], "  action: Guidance line one.");
    assert_eq!(lines[2], "          Guidance line two.");

    // One disposition; `action_excerpt` is trace detail, not a second one.
    assert_eq!(outcome.disposition, NotePackDisposition::Injected);
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Full));
    assert_eq!(outcome.estimated_rendered_chars, Some(rendered.len()));
}

// ── AC7: the motivating note, measured ─────────────────────────────────────
//
// Two fixtures, because the honest answer needs both.
//
// `tool_schema_note_real.md` is the production note's `content` field byte for
// byte (6879 B, fetched from the live corpus). `tool_schema_note_reauthored.md`
// is the same bytes with ONE structural edit — a `## Prevention` heading
// inserted before the regen block and a `## Notes` heading after it — and it
// says so in a comment on its first line. Nothing else differs; the five
// bullets are byte-identical between the two files.
//
// The split exists because the real note has NO ATX heading of any level. The
// proposal's objective names this note, but no deterministic extractor keyed
// on the authoring template can reach its commands until it is re-authored.
// Asserting that on the real body keeps the gap visible instead of hiding it
// behind an invented fixture.

const TOOL_SCHEMA_REAL_BODY: &str = include_str!("fixtures/tool_schema_note_real.md");
const TOOL_SCHEMA_REAUTHORED_BODY: &str = include_str!("fixtures/tool_schema_note_reauthored.md");

pub(super) const TOOL_SCHEMA_PERMALINK: &str = "pitfalls/tool-schema-edits-must-regenerate-all-derived-goldens-or-the-merge-queue-breaks-for-everyone";

/// The production note's `retrieval_anchor`, verbatim.
pub(super) const TOOL_SCHEMA_ANCHOR: &str = "Editing MCP tool schemas/param descriptions in the djinn repo, or the merge-queue suite fails on schema snapshot/golden mismatches.";

/// The three regeneration commands the proposal's objective names, as complete
/// source lines, verbatim from the production note — including the test filter
/// on the second one, without which the command does not do the thing.
pub(super) const REGEN_COMMAND_LINES: [&str; 3] = [
    "- Server insta snap: `INSTA_UPDATE=always cargo test --all-features tool_schemas` (in `server/`)",
    "- Corpus fixture: `UPDATE_DJINN_MCP_SERVER_FIXTURE=1 cargo test -p djinn-control-plane --lib server_tests::tests::djinn_mcp_server_corpus_fixture_is_current` → writes `crates/djinn-provider/tests/fixtures/tool_schema_projection/builtin/djinn_mcp_server.json` (fails CI as \"Server Test shard\" — easy to miss because it is NOT named a schema check)",
    "- UI types: `pnpm mcp:types:snapshot` (in `ui/`; reads the server insta snap, so regenerate that FIRST)",
];

fn tool_schema_note(body: &str) -> Note {
    let mut note = base_note(
        TOOL_SCHEMA_PERMALINK,
        "Tool-schema edits must regenerate ALL derived goldens or the merge queue breaks for everyone",
        body,
    );
    note.retrieval_anchor = Some(TOOL_SCHEMA_ANCHOR.to_string());
    note
}

/// The gap, pinned. The real note yields the anchor but NO action excerpt,
/// because it carries no ATX heading for the grammar to key on.
#[test]
fn real_tool_schema_note_yields_the_anchor_but_no_excerpt_today() {
    assert!(
        !TOOL_SCHEMA_REAL_BODY
            .lines()
            .any(|line| line.starts_with("## ") || line.starts_with("# ")),
        "fixture drift: the production note is expected to have no ATX heading"
    );

    let note = tool_schema_note(TOOL_SCHEMA_REAL_BODY);
    let (rendered, outcome) = pack_one(&note, default_config());

    assert!(rendered.contains(TOOL_SCHEMA_ANCHOR), "{rendered}");
    assert_eq!(
        rendered.split('\n').count(),
        1,
        "summary-only until the note is re-authored: {rendered}"
    );
    assert_eq!(outcome.disposition, NotePackDisposition::Injected);
    assert_eq!(
        outcome.action_excerpt, None,
        "no eligible section exists in the real body"
    );
    for command in REGEN_COMMAND_LINES {
        assert!(
            !rendered.contains(command),
            "the real body cannot deliver its commands inline yet"
        );
    }
}

/// AC7 proper: once the note carries the template's heading, the pack contains
/// the actionable content of the note that should have won on 2026-08-05 — all
/// three regeneration commands, complete, under default settings.
#[test]
fn reauthored_tool_schema_note_delivers_all_three_regeneration_commands() {
    let note = tool_schema_note(TOOL_SCHEMA_REAUTHORED_BODY);
    let config = default_config();
    let (rendered, outcome) = pack_one(&note, config);
    let lines: Vec<&str> = rendered.split('\n').collect();

    assert!(
        lines[0].starts_with(&format!("- **[Pitfall] {TOOL_SCHEMA_PERMALINK}**: ")),
        "{}",
        lines[0]
    );
    assert!(lines[0].contains(TOOL_SCHEMA_ANCHOR), "{}", lines[0]);

    // Each command must appear as a COMPLETE physical line, prefix included —
    // `contains` on a fragment would pass on a truncated command.
    for command in REGEN_COMMAND_LINES {
        let expected = format!("          {command}");
        assert!(
            lines.contains(&expected.as_str()),
            "missing or incomplete command line:\n  want: {expected}\n  got:\n{rendered}"
        );
    }
    // The full test filter specifically — dropping it makes the command a no-op.
    assert!(
        rendered.contains("server_tests::tests::djinn_mcp_server_corpus_fixture_is_current"),
        "the corpus-fixture command lost its test filter: {rendered}"
    );

    // The section is 1097 B rendered, over the 1024 B cap, so the tail is
    // dropped at a line boundary and replaced by the pull marker.
    assert_eq!(
        lines.last().copied(),
        Some(marker_for(TOOL_SCHEMA_PERMALINK).as_str()),
        "a truncated excerpt must end in the pull marker: {rendered}"
    );
    assert_eq!(outcome.action_excerpt, Some(ActionExcerptDetail::Truncated));
    assert_eq!(outcome.disposition, NotePackDisposition::Injected);

    // Byte contract holds on the real content under shipped defaults.
    assert!(action_allocation_bytes(&rendered) <= ACTION_EXCERPT_CAP);
    for line in &lines {
        assert!(line.len() <= config.line_byte_cap, "line too long: {line}");
    }
    assert!(rendered.len() <= config.total_byte_budget);
}

/// The cap was derived to deliver exactly this. Pin the derivation so a future
/// cap change that silently drops a command fails here.
#[test]
fn action_cap_is_the_smallest_that_delivers_the_three_commands() {
    let note = tool_schema_note(TOOL_SCHEMA_REAUTHORED_BODY);
    let deliverable = |cap: usize| {
        let (rendered, _) = pack_one(
            &note,
            KnowledgePackConfig {
                line_byte_cap: cap.max(1024),
                ..default_config()
            },
        );
        REGEN_COMMAND_LINES
            .iter()
            .filter(|command| rendered.contains(&format!("          {command}")))
            .count()
    };
    assert_eq!(
        deliverable(ACTION_EXCERPT_CAP),
        3,
        "the shipped cap must deliver all three commands"
    );
    // And the cap is derived, not guessed: it must be at least the rendered
    // size of the intro line, the three commands, and the pull marker. The
    // previously-specified 640 B cap was not, and delivered one command.
    let section: Vec<&str> = TOOL_SCHEMA_REAUTHORED_BODY
        .lines()
        .skip_while(|line| *line != "## Prevention")
        .skip(1)
        .take_while(|line| *line != "## Notes")
        .filter(|line| !line.trim().is_empty())
        .collect();
    let required: usize = section
        .iter()
        .take(4) // intro + the three commands
        .map(|line| "  action: ".len() + line.len() + 1)
        .sum::<usize>()
        + marker_for(TOOL_SCHEMA_PERMALINK).len()
        - 1;
    assert!(
        ACTION_EXCERPT_CAP >= required,
        "cap {ACTION_EXCERPT_CAP} is below the {required} B needed to deliver \
         the intro, all three commands, and the marker"
    );
}
