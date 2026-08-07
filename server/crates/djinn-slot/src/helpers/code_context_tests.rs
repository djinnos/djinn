use super::{
    KnowledgePackConfig, NotePackDisposition, format_knowledge_notes, pack_knowledge_notes,
    pack_ranked_knowledge_notes, rendered_line_overhead_bytes,
};

/// The shortest summary the renderer will ever emit; a line cannot render at
/// all unless its fixed overhead plus this fits `line_byte_cap`.
const MINIMUM_SUMMARY_BYTES: usize = "(no abstract)".len();

/// The largest cap at which `note` still cannot be rendered — one byte below
/// the shortest line it could produce. Derived rather than hard-coded so R1's
/// overhead change cannot silently turn a drop test into a fits test.
fn largest_cap_that_still_drops(note: &Note) -> usize {
    rendered_line_overhead_bytes(note) + MINIMUM_SUMMARY_BYTES - 1
}
use djinn_memory::Note;

// New tests appended below the existing test suite in helpers/tests.rs.

fn fixture_note(
    note_type: &str,
    title: &str,
    permalink: &str,
    abstract_text: Option<&str>,
    overview_text: Option<&str>,
    content: &str,
    confidence: f64,
) -> Note {
    Note {
        id: format!("note:{title}"),
        project_id: "project_test".to_string(),
        permalink: permalink.to_string(),
        title: title.to_string(),
        file_path: String::new(),
        storage: "db".to_string(),
        note_type: note_type.to_string(),
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
        confidence,
        abstract_: abstract_text.map(|s| s.to_string()),
        overview: overview_text.map(|s| s.to_string()),
        scope_paths: "[]".to_string(),
    }
}
#[test]
fn format_knowledge_notes_appends_permalink_on_each_line() {
    // Two notes - different types, distinct permalinks - must each surface
    // their permalink on the rendered line and still preserve the existing
    // type / title / summary shape so the prompt's meaning is unchanged.
    let notes = vec![
        fixture_note(
            "pitfall",
            "Refinement target-less",
            "pitfalls/refinement-target-less",
            Some("Refinements on proposals without a target project die as opaque agent_failure."),
            None,
            "Long body content that should NOT appear because abstract wins.",
            0.5,
        ),
        fixture_note(
            "pattern",
            "Anchor Note",
            "patterns/anchor",
            Some("Use anchors for retrieval."),
            None,
            "Body remains separate from the retrieval anchor.",
            0.9,
        ),
    ];

    let rendered = format_knowledge_notes(&notes, 2000);

    assert!(
        rendered.contains(
            "**[Pitfall] pitfalls/refinement-target-less**: Refinements on proposals without a target project die as opaque agent_failure.",
        ),
        "expected pitfall line with permalink, got: {rendered}"
    );
    assert!(
        rendered.contains("**[Pattern] patterns/anchor**: Use anchors for retrieval."),
        "expected pattern line with permalink, got: {rendered}"
    );
    assert!(
        !rendered.contains("Long body content that should NOT appear"),
        "body content leaked past abstract selection: {rendered}"
    );
}

#[test]
fn format_knowledge_notes_permalink_visible_when_line_fits_within_budget() {
    let notes = vec![fixture_note(
        "case",
        "Sample Case",
        "cases/sample-case",
        Some("Short case abstract."),
        None,
        "Body text.",
        0.6,
    )];

    let rendered = format_knowledge_notes(&notes, 2000);
    assert_eq!(
        rendered,
        "- **[Case] cases/sample-case**: Short case abstract."
    );
}

#[test]
fn format_knowledge_notes_empty_input_returns_empty_string() {
    let rendered = format_knowledge_notes(&[], 2000);
    assert!(
        rendered.is_empty(),
        "expected empty output, got: {rendered:?}"
    );
}

#[test]
fn format_knowledge_notes_budget_counts_permalink_in_truncation() {
    let notes = vec![
        fixture_note("note", "short", "a/short", Some("a"), None, "", 0.5),
        fixture_note(
            "note",
            "medium-summary-text",
            "b/medium-summary",
            Some("b"),
            None,
            "",
            0.5,
        ),
    ];
    let first_line = "- **[Note] a/short**: a";
    let second_line = "- **[Note] b/medium-summary**: b";
    let used_after_first = first_line.len() + 1;
    let budget = used_after_first + second_line.len() - 1;

    let rendered = format_knowledge_notes(&notes, budget);
    assert_eq!(
        rendered, first_line,
        "expected only the first line (with permalink) within budget, got: {rendered:?}"
    );
    assert!(
        !rendered.contains("b/medium-summary"),
        "second note's permalink leaked past budget: {rendered}"
    );
}

#[test]
fn format_knowledge_notes_budget_rejects_line_whose_permalink_itself_overflows() {
    let notes = vec![fixture_note(
        "pattern",
        "Long",
        "patterns/this-permalink-slug-is-intentionally-very-long-on-purpose",
        Some("summary"),
        None,
        "",
        0.5,
    )];

    let rendered = format_knowledge_notes(&notes, largest_cap_that_still_drops(&notes[0]));
    assert!(
        rendered.is_empty(),
        "single-line overflow must drop the note rather than partial-emit, got: {rendered:?}"
    );
    assert!(
        !rendered.contains("patterns/this-permalink-slug"),
        "overflow line must not leak, got: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// pack_knowledge_notes tests
// ---------------------------------------------------------------------------

#[test]
fn pack_knowledge_notes_rendered_matches_format_knowledge_notes() {
    // The rendered output of pack_knowledge_notes must be byte-identical to
    // format_knowledge_notes for the same inputs, at every budget size.
    let notes = vec![
        fixture_note(
            "pitfall",
            "Refinement target-less",
            "pitfalls/refinement-target-less",
            Some("Refinements on proposals without a target project die as opaque agent_failure."),
            None,
            "Long body content that should NOT appear because abstract wins.",
            0.5,
        ),
        fixture_note(
            "pattern",
            "Anchor Note",
            "patterns/anchor",
            Some("Use anchors for retrieval."),
            None,
            "Body remains separate from the retrieval anchor.",
            0.9,
        ),
    ];

    // Generous budget: both fit.
    assert_eq!(
        pack_knowledge_notes(&notes, 2000).rendered,
        format_knowledge_notes(&notes, 2000),
    );

    // Tight budget: only the first entry fits. Derived from what the renderer
    // actually emits, so R1's overhead change cannot make this stale.
    let budget = format_knowledge_notes(&notes[..1], 2000).len();
    assert_eq!(
        pack_knowledge_notes(&notes, budget).rendered,
        format_knowledge_notes(&notes, budget),
    );

    // Zero budget: nothing fits.
    assert_eq!(
        pack_knowledge_notes(&notes, 0).rendered,
        format_knowledge_notes(&notes, 0),
    );
}

#[test]
fn pack_knowledge_notes_empty_input_returns_empty() {
    let packed = pack_knowledge_notes(&[], 2000);
    assert!(packed.rendered.is_empty(), "expected empty rendered text");
    assert!(packed.outcomes.is_empty(), "expected empty outcomes");
    assert_eq!(packed.total_injected_chars, 0);
    assert_eq!(packed.total_injected_tokens, 0);
}

#[test]
fn pack_knowledge_notes_all_injected_when_budget_generous() {
    let notes = vec![
        fixture_note(
            "pitfall",
            "Pit One",
            "pitfalls/one",
            Some("Abstract one."),
            None,
            "",
            0.5,
        ),
        fixture_note(
            "pattern",
            "Pat Two",
            "patterns/two",
            Some("Abstract two."),
            None,
            "",
            0.5,
        ),
        fixture_note(
            "case",
            "Case Three",
            "cases/three",
            Some("Abstract three."),
            None,
            "",
            0.5,
        ),
    ];

    let packed = pack_knowledge_notes(&notes, 5000);
    assert_eq!(packed.outcomes.len(), 3);
    for outcome in &packed.outcomes {
        assert_eq!(outcome.disposition, NotePackDisposition::Injected);
        assert!(
            outcome.estimated_rendered_chars.is_some(),
            "injected note must have char estimate"
        );
        assert!(
            outcome.estimated_rendered_tokens.is_some(),
            "injected note must have token estimate"
        );
    }
    assert!(packed.total_injected_chars > 0);
    assert!(packed.total_injected_tokens > 0);
}

#[test]
fn pack_knowledge_notes_budget_prunes_first_overflow_and_all_subsequent() {
    let notes = vec![
        fixture_note("note", "short", "a/short", Some("a"), None, "", 0.5),
        fixture_note(
            "note",
            "medium-summary-text",
            "b/medium-summary",
            Some("b"),
            None,
            "",
            0.5,
        ),
        fixture_note("note", "third-note", "c/third", Some("c"), None, "", 0.5),
    ];

    // Budget only fits the first line.
    let first_line = "- **[Note] a/short**: a";
    let budget = first_line.len();

    let packed = pack_knowledge_notes(&notes, budget);
    assert_eq!(packed.outcomes.len(), 3);

    // First note injected.
    assert_eq!(
        packed.outcomes[0].disposition,
        NotePackDisposition::OversizedSkipped
    );
    assert_eq!(packed.outcomes[0].permalink, "a/short");
    assert_eq!(packed.outcomes[0].title, "short");

    // Second note budget-pruned (first overflow).
    assert_eq!(
        packed.outcomes[1].disposition,
        NotePackDisposition::OversizedSkipped
    );
    assert_eq!(packed.outcomes[1].permalink, "b/medium-summary");
    assert_eq!(packed.outcomes[1].title, "medium-summary-text");
    assert!(packed.outcomes[1].estimated_rendered_chars.is_none());
    assert!(packed.outcomes[1].estimated_rendered_tokens.is_none());

    // Third note also budget-pruned (cascade after first overflow).
    assert_eq!(
        packed.outcomes[2].disposition,
        NotePackDisposition::OversizedSkipped
    );
    assert_eq!(packed.outcomes[2].permalink, "c/third");

    // Rendered text only has the first note.
    assert_eq!(packed.rendered, String::new());
}

#[test]
fn pack_knowledge_notes_zero_budget_prunes_all() {
    let notes = vec![
        fixture_note("note", "A", "a/a", Some("a"), None, "", 0.5),
        fixture_note("note", "B", "b/b", Some("b"), None, "", 0.5),
    ];

    let packed = pack_knowledge_notes(&notes, 0);
    assert_eq!(packed.outcomes.len(), 2);
    for outcome in &packed.outcomes {
        assert_eq!(outcome.disposition, NotePackDisposition::OversizedSkipped);
        assert!(outcome.estimated_rendered_chars.is_none());
        assert!(outcome.estimated_rendered_tokens.is_none());
    }
    assert!(packed.rendered.is_empty());
    assert_eq!(packed.total_injected_chars, 0);
    assert_eq!(packed.total_injected_tokens, 0);
}

#[test]
fn pack_knowledge_notes_outcome_metadata_matches_permalink_and_title() {
    let notes = vec![
        fixture_note(
            "pitfall",
            "Refinement target-less",
            "pitfalls/refinement-target-less",
            Some("Refinements on proposals without a target project die as opaque agent_failure."),
            None,
            "",
            0.5,
        ),
        fixture_note(
            "pattern",
            "Anchor Note",
            "patterns/anchor",
            Some("Use anchors for retrieval."),
            None,
            "",
            0.9,
        ),
    ];

    let packed = pack_knowledge_notes(&notes, 2000);
    assert_eq!(
        packed.outcomes[0].permalink,
        "pitfalls/refinement-target-less"
    );
    assert_eq!(packed.outcomes[0].title, "Refinement target-less");
    assert_eq!(packed.outcomes[1].permalink, "patterns/anchor");
    assert_eq!(packed.outcomes[1].title, "Anchor Note");
}

#[test]
fn pack_knowledge_notes_injected_char_estimate_matches_rendered_line_length() {
    let notes = vec![fixture_note(
        "case",
        "Sample Case",
        "cases/sample-case",
        Some("Short case abstract."),
        None,
        "Body text.",
        0.6,
    )];

    let packed = pack_knowledge_notes(&notes, 2000);
    let expected_line = "- **[Case] cases/sample-case**: Short case abstract.";
    assert_eq!(packed.rendered, expected_line);
    assert_eq!(
        packed.outcomes[0].estimated_rendered_chars,
        Some(expected_line.len()),
        "char estimate must match the actual rendered line length"
    );
}

#[test]
fn pack_knowledge_notes_token_estimate_is_ceil_of_chars_divided_by_four() {
    let notes = vec![fixture_note(
        "note",
        "Tok",
        "t/tok",
        Some("x"),
        None,
        "",
        0.5,
    )];

    let packed = pack_knowledge_notes(&notes, 2000);
    let chars = packed.outcomes[0].estimated_rendered_chars.unwrap();
    let expected_tokens = ((chars as f64) / 4.0).ceil() as usize;
    assert_eq!(
        packed.outcomes[0].estimated_rendered_tokens,
        Some(expected_tokens),
        "token estimate must be ceil(chars / 4.0)"
    );
    // Verify aggregate totals are consistent.
    assert_eq!(packed.total_injected_chars, chars); // no newline around a single line
    let expected_total_tokens = ((packed.total_injected_chars as f64) / 4.0).ceil() as usize;
    assert_eq!(packed.total_injected_tokens, expected_total_tokens);
}

#[test]
fn pack_knowledge_notes_budget_permalink_overflow_prunes() {
    // Mirrors the existing format_knowledge_notes_budget_rejects_line_whose_permalink_itself_overflows
    // test, ensuring pack_knowledge_notes behaves identically.
    let notes = vec![fixture_note(
        "pattern",
        "Long",
        "patterns/this-permalink-slug-is-intentionally-very-long-on-purpose",
        Some("summary"),
        None,
        "",
        0.5,
    )];

    let packed = pack_knowledge_notes(&notes, largest_cap_that_still_drops(&notes[0]));
    assert!(
        packed.rendered.is_empty(),
        "single-line overflow must drop the note, got: {:?}",
        packed.rendered
    );
    assert_eq!(packed.outcomes.len(), 1);
    assert_eq!(
        packed.outcomes[0].disposition,
        NotePackDisposition::OversizedSkipped
    );
    assert!(packed.outcomes[0].estimated_rendered_chars.is_none());
}

/// Regression: once the budget is exhausted, subsequent notes must be
/// classified as budget-pruned **without** computing their label, summary,
/// or rendered line content.  The old buggy version would continue
/// evaluating the fallback summary for later notes, panicking on notes
/// whose `content[..min(100)]` lands on a non-UTF-8 byte boundary.
#[test]
fn pack_knowledge_notes_budget_exhausted_skips_content_for_later_notes() {
    // Note 1: overflows budget → triggers budget_exhausted.
    let notes = vec![
        fixture_note(
            "note",
            "overflow",
            "a/overflow",
            Some("This abstract is intentionally long enough to overflow the tiny budget."),
            None,
            "",
            0.5,
        ),
        // Note 2: no abstract/overview, content whose byte 100 is a
        // non-UTF-8 boundary.  The fallback summary `content[..min(100)]`
        // would panic if reached.
        fixture_note(
            "note",
            "utf8-trap",
            "b/trap",
            None,
            None,
            &("a".repeat(99) + "é"), // byte index 100 = inside 'é' (2 bytes)
            0.3,
        ),
    ];

    // Tiny budget; nothing fits. R1 halved per-line overhead, so this had to
    // shrink from 50 to stay below the shortest renderable line.
    let budget = 30;
    let packed = pack_knowledge_notes(&notes, budget);

    assert_eq!(packed.outcomes.len(), 2);
    // Both notes must be budget-pruned.
    assert_eq!(
        packed.outcomes[0].disposition,
        NotePackDisposition::OversizedSkipped
    );
    assert_eq!(
        packed.outcomes[1].disposition,
        NotePackDisposition::OversizedSkipped
    );
    // Rendered output is empty.
    assert!(packed.rendered.is_empty());
    // Crucially: the function must not panic on note 2's non-UTF-8 boundary.
}

#[test]
fn ranked_packer_partitions_universe_and_skips_oversized_rank_one() {
    let notes = vec![
        fixture_note("note", "low", "a/low", Some("a"), None, "", 0.2),
        // R1: overhead is now driven by the permalink, not the title, so the
        // oversized candidate is one whose *permalink* cannot fit the cap.
        fixture_note(
            "note",
            "large",
            &format!("b/{}", "x".repeat(100)),
            Some("b"),
            None,
            "",
            0.9,
        ),
        fixture_note("note", "fits", "c/fits", Some("c"), None, "", 0.8),
        fixture_note("note", "outside", "d/out", Some("d"), None, "", 0.7),
    ];
    let packed = pack_ranked_knowledge_notes(
        &notes,
        KnowledgePackConfig {
            minimum_confidence: 0.3,
            top_k: 2,
            total_byte_budget: 200,
            line_byte_cap: 100,
        },
    );
    assert_eq!(
        packed
            .outcomes
            .iter()
            .map(|o| &o.disposition)
            .collect::<Vec<_>>(),
        vec![
            &NotePackDisposition::ConfidenceFiltered,
            &NotePackDisposition::OversizedSkipped,
            &NotePackDisposition::Injected,
            &NotePackDisposition::NotTopK,
        ]
    );
    assert_eq!(packed.outcomes.len(), notes.len());
    assert!(packed.rendered.contains("fits"));
}

#[test]
fn ranked_packer_injects_rank_eleven_after_ten_oversized_top_k_notes() {
    let mut notes = (0..10)
        .map(|i| {
            // R1: oversized-ness now comes from the permalink, not the title.
            fixture_note(
                "note",
                &format!("rank {i}"),
                &format!("a/{}-{i}", "x".repeat(80)),
                Some("x"),
                None,
                "",
                1.0,
            )
        })
        .collect::<Vec<_>>();
    notes.push(fixture_note(
        "note",
        "rank eleven",
        "a/eleven",
        Some("fits"),
        None,
        "",
        1.0,
    ));
    let packed = pack_ranked_knowledge_notes(
        &notes,
        KnowledgePackConfig {
            minimum_confidence: 0.0,
            top_k: 11,
            total_byte_budget: 200,
            line_byte_cap: 100,
        },
    );
    assert_eq!(
        packed
            .outcomes
            .iter()
            .filter(|o| o.disposition == NotePackDisposition::OversizedSkipped)
            .count(),
        10
    );
    assert_eq!(
        packed.outcomes[10].disposition,
        NotePackDisposition::Injected
    );
    // R1: the line is labelled by permalink, so identify the survivor by it.
    assert!(packed.rendered.contains("a/eleven"));
}

#[test]
fn ranked_packer_continues_after_budget_miss_and_charges_newlines_exactly() {
    let first = fixture_note(
        "note",
        "first",
        "a/first",
        Some(&"a".repeat(50)),
        None,
        "",
        1.0,
    );
    let medium = fixture_note(
        "note",
        "medium",
        "a/medium",
        Some(&"b".repeat(40)),
        None,
        "",
        1.0,
    );
    let small = fixture_note("note", "small", "a/small", Some("c"), None, "", 1.0);
    let open = KnowledgePackConfig {
        minimum_confidence: 0.0,
        top_k: 1,
        total_byte_budget: 500,
        line_byte_cap: 500,
    };
    let budget = pack_ranked_knowledge_notes(std::slice::from_ref(&first), open)
        .rendered
        .len()
        + 1
        + pack_ranked_knowledge_notes(std::slice::from_ref(&small), open)
            .rendered
            .len();
    let packed = pack_ranked_knowledge_notes(
        &[first, medium, small],
        KnowledgePackConfig {
            minimum_confidence: 0.0,
            top_k: 3,
            total_byte_budget: budget,
            line_byte_cap: 500,
        },
    );
    assert_eq!(
        packed.outcomes[1].disposition,
        NotePackDisposition::BudgetPruned
    );
    assert_eq!(
        packed.outcomes[2].disposition,
        NotePackDisposition::Injected
    );
    assert_eq!(packed.total_injected_chars, packed.rendered.len());
    assert_eq!(packed.rendered.matches('\n').count(), 1);
}

#[test]
fn ranked_packer_uses_l0_fallback_and_utf8_safe_summary_truncation() {
    let note = fixture_note(
        "note",
        "é",
        "a/e",
        Some(" \n "),
        Some("OVERVIEW MUST NOT APPEAR"),
        " éééééééééé ",
        1.0,
    );
    // R1: the rendered line labels with the permalink, not the title.
    let metadata = "- **[Note] a/e**: ".len();
    let packed = pack_ranked_knowledge_notes(
        &[note],
        KnowledgePackConfig {
            minimum_confidence: 0.0,
            top_k: 1,
            total_byte_budget: metadata + 13,
            line_byte_cap: metadata + 13,
        },
    );
    assert!(packed.rendered.contains("ééééé…"));
    assert!(!packed.rendered.contains("OVERVIEW"));
    assert_eq!(packed.rendered.len(), metadata + 13);
    let blank = fixture_note("note", "blank", "a/blank", Some(" \n "), None, " \t ", 1.0);
    let fallback = pack_ranked_knowledge_notes(
        &[blank],
        KnowledgePackConfig {
            minimum_confidence: 0.0,
            top_k: 1,
            total_byte_budget: 200,
            line_byte_cap: 200,
        },
    );
    assert!(fallback.rendered.contains("(no abstract)"));
}

#[test]
fn ranked_packer_exhaustive_partition_and_non_starvation_property() {
    let notes = vec![
        fixture_note("note", &"x".repeat(90), "a/large", Some("x"), None, "", 0.9),
        fixture_note("note", "small", "a/small", Some("small"), None, "", 0.8),
        fixture_note("note", "low", "a/low", Some("low"), None, "", 0.1),
    ];
    for line_byte_cap in 0..=120 {
        for total_byte_budget in 0..=120 {
            let config = KnowledgePackConfig {
                minimum_confidence: 0.3,
                top_k: 2,
                total_byte_budget,
                line_byte_cap,
            };
            let packed = pack_ranked_knowledge_notes(&notes, config);
            assert_eq!(packed.outcomes.len(), notes.len());
            assert!(packed.rendered.len() <= total_byte_budget);
            assert!(
                packed
                    .rendered
                    .lines()
                    .all(|line| line.len() <= line_byte_cap)
            );
            let any_fits_empty_budget = notes[..2].iter().any(|note| {
                !pack_ranked_knowledge_notes(
                    std::slice::from_ref(note),
                    KnowledgePackConfig { top_k: 1, ..config },
                )
                .rendered
                .is_empty()
            });
            if any_fits_empty_budget {
                assert!(
                    packed
                        .outcomes
                        .iter()
                        .any(|outcome| outcome.disposition == NotePackDisposition::Injected),
                    "a scoped candidate fits at cap={line_byte_cap}, budget={total_byte_budget}"
                );
            }
        }
    }
}

// ── Proposal 5205: single-pass packing of one ranked candidate list ─────────

mod no_backfill {
    use super::super::{KnowledgePackConfig, NotePackDisposition, pack_ranked_knowledge_notes};
    use super::{fixture_note, largest_cap_that_still_drops};
    use djinn_memory::Note;

    /// A note whose fixed per-line overhead alone exceeds any sane line cap,
    /// so it can never render at any budget.
    fn oversized_note(index: usize) -> Note {
        let permalink = format!("pitfalls/{}", "x".repeat(400));
        fixture_note(
            "pitfall",
            &format!("oversized-{index}"),
            &permalink,
            Some("an oversized candidate"),
            None,
            "body",
            1.0,
        )
    }

    fn ordinary_note(index: usize) -> Note {
        fixture_note(
            "pitfall",
            &format!("ordinary-{index}"),
            &format!("pitfalls/ordinary-{index}"),
            Some("a small candidate"),
            None,
            "body",
            1.0,
        )
    }

    fn low_confidence_note(index: usize) -> Note {
        fixture_note(
            "pitfall",
            &format!("weak-{index}"),
            &format!("pitfalls/weak-{index}"),
            Some("below the floor"),
            None,
            "body",
            0.1,
        )
    }

    fn dispositions(
        notes: &[Note],
        config: KnowledgePackConfig,
    ) -> Vec<(String, NotePackDisposition)> {
        pack_ranked_knowledge_notes(notes, config)
            .outcomes
            .into_iter()
            .map(|outcome| (outcome.permalink, outcome.disposition))
            .collect()
    }

    /// AC6's motivating fixture: an oversized note *inside* top-k must not
    /// promote the relevant note sitting at fused rank `top_k`. That note stays
    /// `NotTopK`, and the injected count is 2, not 3.
    #[test]
    fn oversized_note_in_top_k_does_not_promote_the_next_candidate() {
        let notes = vec![
            ordinary_note(0),
            oversized_note(1),
            ordinary_note(2),
            // The relevant, byte-fitting note at zero-based fused rank 3 == top_k.
            ordinary_note(3),
        ];
        let config = KnowledgePackConfig {
            minimum_confidence: 0.3,
            top_k: 3,
            total_byte_budget: 8192,
            line_byte_cap: 256,
        };
        assert!(
            largest_cap_that_still_drops(&notes[1]) >= config.line_byte_cap,
            "fixture must actually be oversized at this line cap"
        );

        let outcomes = dispositions(&notes, config);
        assert_eq!(
            outcomes,
            vec![
                (
                    "pitfalls/ordinary-0".to_string(),
                    NotePackDisposition::Injected
                ),
                (
                    format!("pitfalls/{}", "x".repeat(400)),
                    NotePackDisposition::OversizedSkipped
                ),
                (
                    "pitfalls/ordinary-2".to_string(),
                    NotePackDisposition::Injected
                ),
                (
                    "pitfalls/ordinary-3".to_string(),
                    NotePackDisposition::NotTopK
                ),
            ]
        );
    }

    /// The confidence floor is evaluated *before* top-k, so a filtered note
    /// does not consume one of the `top_k` slots.
    #[test]
    fn confidence_filtered_notes_do_not_consume_top_k_slots() {
        let notes = vec![
            low_confidence_note(0),
            ordinary_note(1),
            ordinary_note(2),
            ordinary_note(3),
        ];
        let outcomes = dispositions(
            &notes,
            KnowledgePackConfig {
                minimum_confidence: 0.3,
                top_k: 2,
                total_byte_budget: 8192,
                line_byte_cap: 256,
            },
        );
        assert_eq!(
            outcomes
                .iter()
                .map(|(_, disposition)| disposition.clone())
                .collect::<Vec<_>>(),
            vec![
                NotePackDisposition::ConfidenceFiltered,
                NotePackDisposition::Injected,
                NotePackDisposition::Injected,
                NotePackDisposition::NotTopK,
            ]
        );
    }

    /// Every one of the at-most-50 candidates receives exactly one terminal
    /// disposition, and the packed bytes never exceed the configured ceiling.
    #[test]
    fn every_candidate_in_a_full_window_gets_exactly_one_disposition() {
        let notes: Vec<Note> = (0..50).map(ordinary_note).collect();
        let config = KnowledgePackConfig {
            minimum_confidence: 0.3,
            top_k: 10,
            total_byte_budget: 400,
            line_byte_cap: 256,
        };
        let packed = pack_ranked_knowledge_notes(&notes, config);

        assert_eq!(packed.outcomes.len(), notes.len());
        for (note, outcome) in notes.iter().zip(&packed.outcomes) {
            assert_eq!(outcome.permalink, note.permalink);
        }
        let injected = packed
            .outcomes
            .iter()
            .filter(|outcome| outcome.disposition == NotePackDisposition::Injected)
            .count();
        let not_top_k = packed
            .outcomes
            .iter()
            .filter(|outcome| outcome.disposition == NotePackDisposition::NotTopK)
            .count();
        assert_eq!(not_top_k, 40, "everything past top-k is NotTopK");
        assert!(injected > 0 && injected <= config.top_k);
        assert!(
            packed.total_injected_chars <= config.total_byte_budget,
            "packed bytes must respect the ceiling"
        );
    }

    /// Repeated runs against the same inputs produce identical ordered IDs,
    /// dispositions, and byte counts.
    #[test]
    fn packing_is_deterministic_across_repeats() {
        let notes: Vec<Note> = (0..20).map(ordinary_note).collect();
        let config = KnowledgePackConfig {
            minimum_confidence: 0.3,
            top_k: 7,
            total_byte_budget: 500,
            line_byte_cap: 256,
        };
        let first = pack_ranked_knowledge_notes(&notes, config);
        let second = pack_ranked_knowledge_notes(&notes, config);
        assert_eq!(first.rendered, second.rendered);
        assert_eq!(first.total_injected_chars, second.total_injected_chars);
        assert_eq!(
            first
                .outcomes
                .iter()
                .map(|outcome| (outcome.permalink.clone(), outcome.disposition.clone()))
                .collect::<Vec<_>>(),
            second
                .outcomes
                .iter()
                .map(|outcome| (outcome.permalink.clone(), outcome.disposition.clone()))
                .collect::<Vec<_>>(),
        );
    }
}

// ── Proposal 5205: base-tree-validated task scope derivation ────────────────

mod validated_scope {
    use super::super::{
        BaseTreeProvider, ListedBaseTree, ScopeFallbackReason, derive_task_scope_path_tokens,
        derive_task_scope_paths, normalize_scope_token, resolve_scope_token,
    };

    /// A minimal `Task` built without touching the database, so scope
    /// derivation is testable in any environment.
    fn scope_task(description: &str, design: &str) -> djinn_core::models::Task {
        djinn_core::models::Task {
            escalation_evidence_at: None,
            id: "task-5205".to_string(),
            project_id: "project-5205".to_string(),
            short_id: "t-5205".to_string(),
            epic_id: None,
            title: String::new(),
            description: description.to_string(),
            design: design.to_string(),
            issue_type: "task".to_string(),
            status: "in_progress".to_string(),
            priority: 1,
            owner: "test-owner".to_string(),
            labels: "[]".to_string(),
            acceptance_criteria: "[]".to_string(),
            reopen_count: 0,
            continuation_count: 0,
            total_reopen_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".to_string(),
            agent_type: None,
            execution_context: None,
            created_by_user_id: "fixture-user".into(),
            ci_status: "unknown".to_string(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".to_string(),
            ci_primary_blocking_check: None,
            ci_failure_annotations: None,
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            ci_mq_state: None,
            ci_mq_run_id: None,
            ci_mq_head_sha: None,
            ci_mq_failed_check_names: None,
            ci_mq_failure_fingerprint: None,
            ci_mq_same_signature_count: None,
            ci_mq_first_seen_at: None,
            ci_mq_last_seen_at: None,
            unresolved_blocker_count: 0,
            refinement_run_id: None,
            refinement_intent_id: None,
            refinement_generation: None,
            refinement_round: None,
            refinement_phase: None,
            refinement_role: None,
        }
    }

    /// A synthetic base revision. Every path here is invented for this test;
    /// nothing is read from any deployment's repository or database.
    fn base_tree() -> ListedBaseTree {
        ListedBaseTree::from_tracked_files([
            "server/crates/alpha/src/lib.rs",
            "server/crates/alpha/src/engine/mod.rs",
            "server/crates/beta/src/main.rs",
            "ui/components/button.tsx",
            "docs/design.md",
        ])
    }

    #[test]
    fn listed_base_tree_derives_directories_from_tracked_files() {
        let tree = base_tree();
        assert_eq!(tree.file_count(), 5);
        assert!(tree.is_file("server/crates/alpha/src/lib.rs"));
        assert!(tree.is_directory("server/crates/alpha/src"));
        assert!(tree.is_directory("server"));
        // A file is not a directory, and an unknown path is neither.
        assert!(!tree.is_directory("server/crates/alpha/src/lib.rs"));
        assert!(!tree.is_file("server/crates/alpha/src"));
        assert!(!tree.is_directory("server/crates/gamma"));
    }

    // ── normalization ──────────────────────────────────────────────────────

    #[test]
    fn normalization_folds_separators_and_leading_dot_slash() {
        assert_eq!(
            normalize_scope_token("server\\crates\\alpha"),
            Some("server/crates/alpha".to_string())
        );
        assert_eq!(
            normalize_scope_token("./server/crates/alpha"),
            Some("server/crates/alpha".to_string())
        );
        assert_eq!(
            normalize_scope_token("server//crates///alpha"),
            Some("server/crates/alpha".to_string())
        );
        assert_eq!(
            normalize_scope_token("server/crates/alpha/"),
            Some("server/crates/alpha".to_string())
        );
    }

    #[test]
    fn normalization_preserves_git_path_case() {
        assert_eq!(
            normalize_scope_token("UI/Components/Button.tsx"),
            Some("UI/Components/Button.tsx".to_string())
        );
    }

    #[test]
    fn normalization_rejects_absolute_and_traversal_paths() {
        assert_eq!(normalize_scope_token("/etc/passwd"), None);
        assert_eq!(normalize_scope_token("../../etc/passwd"), None);
        assert_eq!(normalize_scope_token("server/../../etc"), None);
        assert_eq!(normalize_scope_token("server/./crates"), None);
        assert_eq!(normalize_scope_token("   "), None);
    }

    // ── resolution ─────────────────────────────────────────────────────────

    #[test]
    fn existing_file_resolves_to_itself() {
        let tree = base_tree();
        assert_eq!(
            resolve_scope_token("server/crates/alpha/src/lib.rs", &tree),
            Some("server/crates/alpha/src/lib.rs".to_string())
        );
    }

    #[test]
    fn existing_directory_resolves_to_itself() {
        let tree = base_tree();
        assert_eq!(
            resolve_scope_token("server/crates/alpha/src", &tree),
            Some("server/crates/alpha/src".to_string())
        );
    }

    #[test]
    fn new_path_resolves_to_its_longest_existing_ancestor() {
        let tree = base_tree();
        // A file that does not exist yet, under a directory that does.
        assert_eq!(
            resolve_scope_token("server/crates/alpha/src/engine/planner.rs", &tree),
            Some("server/crates/alpha/src/engine".to_string())
        );
        // Several missing components collapse to the deepest real ancestor.
        assert_eq!(
            resolve_scope_token("server/crates/alpha/src/a/b/c/d.rs", &tree),
            Some("server/crates/alpha/src".to_string())
        );
    }

    #[test]
    fn renamed_or_wholly_new_subtree_degrades_to_a_coarse_real_ancestor() {
        let tree = base_tree();
        // `gamma` never existed at the base revision; `server/crates` did.
        assert_eq!(
            resolve_scope_token("server/crates/gamma/src/lib.rs", &tree),
            Some("server/crates".to_string())
        );
    }

    #[test]
    fn deleted_path_still_resolves_because_it_existed_at_the_base_revision() {
        // The base tree is the immutable revision the attempt branched from, so
        // a path deleted on the branch is still a valid scope anchor.
        let tree = base_tree();
        assert_eq!(
            resolve_scope_token("docs/design.md", &tree),
            Some("docs/design.md".to_string())
        );
    }

    #[test]
    fn token_with_no_existing_ancestor_is_discarded() {
        let tree = base_tree();
        assert_eq!(resolve_scope_token("accept/reject", &tree), None);
        assert_eq!(resolve_scope_token("Pod/Job", &tree), None);
        assert_eq!(resolve_scope_token("and/or", &tree), None);
    }

    #[test]
    fn repository_root_is_never_emitted_as_a_scope_path() {
        // `nonexistent` has no existing ancestor other than the root itself.
        let tree = base_tree();
        assert_eq!(resolve_scope_token("nonexistent/child", &tree), None);
    }

    // ── end-to-end derivation ──────────────────────────────────────────────

    #[test]
    fn prose_junk_is_rejected_while_real_paths_survive() {
        let tree = base_tree();
        let task = scope_task(
            "We must accept/reject the Pod/Job and/or retry. \
             Touch `server/crates/alpha/src/lib.rs` and ui/components/button.tsx now.",
            "Also add server/crates/alpha/src/engine/planner.rs (new file).",
        );

        let derived = derive_task_scope_paths(&task, None, Some(&tree));

        assert_eq!(derived.fallback_reason, None);
        assert_eq!(
            derived.paths,
            vec![
                "server/crates/alpha/src/engine".to_string(),
                "server/crates/alpha/src/lib.rs".to_string(),
                "ui/components/button.tsx".to_string(),
            ],
            "junk pairs must be absent and the new file must resolve to its parent"
        );

        // The pre-5205 extractor is what this replaces: it happily emits the
        // junk. Asserting that keeps the regression honest — if validation
        // silently stopped running, this contrast would collapse.
        let unvalidated = derive_task_scope_path_tokens(&task, None);
        assert!(
            unvalidated.iter().any(|path| path == "accept/reject"),
            "unvalidated extraction still yields prose junk: {unvalidated:?}"
        );
    }

    #[test]
    fn epic_context_paths_are_validated_too() {
        let tree = base_tree();
        let task = scope_task("No paths here.", "");
        let derived = derive_task_scope_paths(
            &task,
            Some("Epic touches server/crates/beta/src/main.rs and nope/nothing."),
            Some(&tree),
        );
        assert_eq!(
            derived.paths,
            vec!["server/crates/beta/src/main.rs".to_string()]
        );
    }

    #[test]
    fn provider_unavailability_yields_empty_scope_with_a_typed_reason() {
        let task = scope_task(
            "Touch server/crates/alpha/src/lib.rs and accept/reject.",
            "",
        );
        let derived = derive_task_scope_paths(&task, None, None);
        assert!(
            derived.paths.is_empty(),
            "unvalidated regex tokens must never be trusted"
        );
        assert_eq!(
            derived.fallback_reason,
            Some(ScopeFallbackReason::TreeProviderUnavailable)
        );
    }

    #[test]
    fn an_empty_tree_discards_every_token_without_claiming_a_fallback() {
        // A reachable-but-empty tree is a legitimate zero-result derivation,
        // not a provider failure; traces must be able to tell them apart.
        let tree = ListedBaseTree::from_tracked_files(Vec::<String>::new());
        let task = scope_task("Touch server/crates/alpha/src/lib.rs.", "");
        let derived = derive_task_scope_paths(&task, None, Some(&tree));
        assert!(derived.paths.is_empty());
        assert_eq!(derived.fallback_reason, None);
    }

    #[test]
    fn derived_paths_are_deduplicated_and_deterministically_ordered() {
        let tree = base_tree();
        let task = scope_task(
            "ui/components/button.tsx and again ui/components/button.tsx \
             plus server/crates/beta/src/main.rs here.",
            "server/crates/beta/src/main.rs once more.",
        );
        let derived = derive_task_scope_paths(&task, None, Some(&tree));
        assert_eq!(
            derived.paths,
            vec![
                "server/crates/beta/src/main.rs".to_string(),
                "ui/components/button.tsx".to_string(),
            ]
        );
    }

    #[test]
    fn traversal_and_absolute_tokens_never_reach_the_tree() {
        struct PanickingTree;
        impl BaseTreeProvider for PanickingTree {
            fn is_file(&self, path: &str) -> bool {
                assert!(
                    !path.contains("..") && !path.starts_with('/'),
                    "unsafe path reached the tree provider: {path}"
                );
                false
            }
            fn is_directory(&self, path: &str) -> bool {
                self.is_file(path)
            }
        }
        // The regex happily extracts `docs/../secrets/key.pem`; normalization
        // must reject it before the tree is ever consulted.
        let task = scope_task("See docs/../secrets/key.pem for context.", "");
        let derived = derive_task_scope_paths(&task, None, Some(&PanickingTree));
        assert!(derived.paths.is_empty());
    }
}
