use super::{
    KnowledgePackConfig, NotePackDisposition, format_knowledge_notes, pack_knowledge_notes,
    pack_ranked_knowledge_notes,
};
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
            "**[Pitfall] Refinement target-less**: Refinements on proposals without a target project die as opaque agent_failure. (permalink: pitfalls/refinement-target-less)",
        ),
        "expected pitfall line with permalink, got: {rendered}"
    );
    assert!(
        rendered.contains(
            "**[Pattern] Anchor Note**: Use anchors for retrieval. (permalink: patterns/anchor)",
        ),
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
        "- **[Case] Sample Case**: Short case abstract. (permalink: cases/sample-case)"
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
    let first_line = "- **[Note] short**: a (permalink: a/short)";
    let second_line = "- **[Note] medium-summary-text**: b (permalink: b/medium-summary)";
    let used_after_first = first_line.len() + 1;
    let budget = used_after_first + second_line.len() - 1;

    let rendered = format_knowledge_notes(&notes, budget);
    assert_eq!(
        rendered, first_line,
        "expected only the first line (with permalink) within budget, got: {rendered:?}"
    );
    assert!(
        !rendered.contains("(permalink: b/medium-summary)"),
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

    let rendered = format_knowledge_notes(&notes, 100);
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

    // Tight budget: only first fits.
    let first_line = "- **[Pitfall] Refinement target-less**: Refinements on proposals without a target project die as opaque agent_failure. (permalink: pitfalls/refinement-target-less)";
    let budget = first_line.len();
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
    let first_line = "- **[Note] short**: a (permalink: a/short)";
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
    let expected_line =
        "- **[Case] Sample Case**: Short case abstract. (permalink: cases/sample-case)";
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

    let packed = pack_knowledge_notes(&notes, 100);
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

    let budget = 50; // tiny budget; nothing fits
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
        fixture_note(
            "note",
            &"x".repeat(100),
            "b/large",
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
            fixture_note(
                "note",
                &format!("{}-{i}", "x".repeat(80)),
                &format!("a/{i}"),
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
    assert!(packed.rendered.contains("rank eleven"));
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
    let budget = pack_ranked_knowledge_notes(&[first.clone()], open)
        .rendered
        .len()
        + 1
        + pack_ranked_knowledge_notes(&[small.clone()], open)
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
    let metadata = "- **[Note] é**: ".len() + " (permalink: a/e)".len();
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
