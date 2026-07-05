use super::{
    MatchOutcome, MatchStrategy, ambiguity_phrase, find_match, fuzzy_replace, line_range_for,
    match_note_for, nearest_miss_score, reindent_replacement,
};
use std::path::Path;

// ── Existing wrapper-level tests (unchanged behaviour) ──────────────────

#[test]
fn rebases_multiline_replacement_using_matched_indentation() {
    let content = "fn main() {\n    match value {\n        Some(x) => {\n            process(x);\n        }\n    }\n}\n";
    let old_text = "match value {\n    Some(x) => {\n        process(x);\n    }\n}";
    let new_text = "match value {\n    Some(x) => {\n        if ready {\n            process(x);\n        }\n    }\n}";

    let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs"))
        .expect("fuzzy replace should succeed");

    assert_eq!(note.as_deref(), Some("(matched with flexible indentation)"));
    assert!(updated.contains(
            "    match value {\n        Some(x) => {\n            if ready {\n                process(x);\n            }\n        }\n    }"
        ));
}

#[test]
fn preserves_later_nested_indent_when_first_replacement_line_is_less_indented() {
    let content = "impl Example {\n        if condition {\n            run();\n        }\n}\n";
    let old_text = "if condition {\n    run();\n}";
    let new_text =
        "if condition {\n    let nested = || {\n        run();\n    };\n    nested();\n}";

    let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs"))
        .expect("fuzzy replace should succeed");

    assert_eq!(note.as_deref(), Some("(matched with flexible indentation)"));
    assert!(updated.contains(
            "        if condition {\n            let nested = || {\n                run();\n            };\n            nested();\n        }"
        ));
}

#[test]
fn reindent_replacement_preserves_internal_relative_indentation() {
    let matched_block = "        if ready {\n            execute();\n        }";
    let replacement =
        "if ready {\n    let nested = || {\n        execute();\n    };\n    nested();\n}";

    assert_eq!(
        reindent_replacement(matched_block, replacement),
        "        if ready {\n            let nested = || {\n                execute();\n            };\n            nested();\n        }"
    );
}

// ── Typed metadata tests ───────────────────────────────────────────────

#[test]
fn exact_match_returns_success_metadata() {
    let content = "hello world\nfoo bar\n";
    let old_text = "world";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::Exact);
    assert_eq!(m.outcome, MatchOutcome::Success);
    assert_eq!(m.candidate_count, 1);
    let br = m.byte_range.expect("exact success has byte range");
    assert_eq!(br.start, 6);
    assert_eq!(br.end, 11);
    let lr = m.line_range.expect("exact success has line range");
    assert_eq!(lr.start, 1);
    assert_eq!(lr.end, 1);
    assert!(!m.reindented);
    assert!(m.nearest_miss.is_none());
    assert!(m.guard_rejected_reason.is_none());
    assert!(m.unicode_splice.is_none());
}

#[test]
fn exact_match_note_is_none() {
    assert!(match_note_for(MatchStrategy::Exact).is_none());
}

#[test]
fn line_trimmed_match_returns_success_metadata() {
    // Content has trailing spaces; old_text does not.
    let content = "let x = 1;   \nlet y = 2;\n";
    let old_text = "let x = 1;\nlet y = 2;";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::LineTrimmed);
    assert_eq!(m.outcome, MatchOutcome::Success);
    assert_eq!(m.candidate_count, 1);
    assert!(m.byte_range.is_some());
    assert!(m.line_range.is_some());
    assert!(!m.reindented);
}

#[test]
fn whitespace_normalized_match_returns_success_metadata() {
    // Content has extra spaces between tokens; old_text has single spaces.
    let content = "fn    main()   {  }";
    let old_text = "fn main() { }";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::WhitespaceNormalized);
    assert_eq!(m.outcome, MatchOutcome::Success);
    assert_eq!(m.candidate_count, 1);
    let br = m.byte_range.expect("whitespace success has byte range");
    // Byte range should cover the matched span in original content.
    assert_eq!(&content[br.start..br.end], "fn    main()   {  }");
    assert!(!m.reindented);
}

#[test]
fn indentation_flexible_match_returns_success_with_reindentation_metadata() {
    // Content is indented more than old_text.
    let content = "fn outer() {\n    if ready {\n        do_thing();\n    }\n}\n";
    let old_text = "if ready {\n    do_thing();\n}";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::IndentationFlexible);
    assert_eq!(m.outcome, MatchOutcome::Success);
    assert_eq!(m.candidate_count, 1);
    let br = m.byte_range.expect("indentation success has byte range");
    // The matched block in the original includes the file's indentation.
    // The byte range also includes the trailing newline of the matched
    // region (the wrapper compensates via `needs_trailing_newline`), so we
    // assert the block up to the closing brace and separately confirm the
    // range extends to cover the last matched line.
    assert!(content[br.start..br.end].starts_with("    if ready {"));
    assert!(
        content[br.start..br.end].contains("do_thing();"),
        "byte range must cover the matched block: {:?}",
        &content[br.start..br.end]
    );
    let lr = m.line_range.expect("indentation success has line range");
    assert_eq!(lr.start, 2);
    assert_eq!(lr.end, 4);
    assert!(
        m.reindented,
        "indentation_flexible success must set the reindentation flag"
    );
}

#[test]
fn ambiguous_match_returns_ambiguity_metadata() {
    let content = "foo\nbar\nfoo\nbaz\n";
    let old_text = "foo";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::Exact);
    assert_eq!(m.outcome, MatchOutcome::Ambiguous);
    assert_eq!(m.candidate_count, 2);
    assert!(m.byte_range.is_none());
    assert!(m.line_range.is_none());
}

#[test]
fn no_match_returns_no_match_metadata() {
    let content = "hello world\n";
    let old_text = "this text does not exist anywhere";

    let m = find_match(content, old_text);

    assert_eq!(m.outcome, MatchOutcome::NoMatch);
    assert_eq!(m.candidate_count, 0);
    assert!(m.byte_range.is_none());
    assert!(m.line_range.is_none());
    // Nearest-miss score is now populated for no-match outcomes.
    let score = m
        .nearest_miss
        .expect("no_match must carry nearest_miss score");
    assert!(
        (0.0..=1.0).contains(&score),
        "score must be in [0, 1]: {score}"
    );
}

#[test]
fn strategy_as_str_returns_stable_identifiers() {
    assert_eq!(MatchStrategy::Exact.as_str(), "exact");
    assert_eq!(MatchStrategy::LineTrimmed.as_str(), "line_trimmed");
    assert_eq!(
        MatchStrategy::WhitespaceNormalized.as_str(),
        "whitespace_normalized"
    );
    assert_eq!(
        MatchStrategy::IndentationFlexible.as_str(),
        "indentation_flexible"
    );
    assert_eq!(
        MatchStrategy::EscapeNormalized.as_str(),
        "escape_normalized"
    );
    assert_eq!(MatchStrategy::TrimmedBoundary.as_str(), "trimmed_boundary");
}

#[test]
fn ambiguity_phrases_match_legacy_wording() {
    assert_eq!(ambiguity_phrase(MatchStrategy::Exact), "in file");
    assert_eq!(
        ambiguity_phrase(MatchStrategy::LineTrimmed),
        "after trimming trailing whitespace"
    );
    assert_eq!(
        ambiguity_phrase(MatchStrategy::WhitespaceNormalized),
        "after whitespace normalization"
    );
    assert_eq!(
        ambiguity_phrase(MatchStrategy::IndentationFlexible),
        "after stripping indentation"
    );
    assert_eq!(
        ambiguity_phrase(MatchStrategy::EscapeNormalized),
        "after escape normalization"
    );
    assert_eq!(
        ambiguity_phrase(MatchStrategy::TrimmedBoundary),
        "after trimming boundary lines"
    );
}

#[test]
fn strategy_order_includes_new_strategies_after_indentation_flexible() {
    let expected: &[MatchStrategy] = &[
        MatchStrategy::Exact,
        MatchStrategy::LineTrimmed,
        MatchStrategy::WhitespaceNormalized,
        MatchStrategy::IndentationFlexible,
        MatchStrategy::EscapeNormalized,
        MatchStrategy::TrimmedBoundary,
    ];
    assert_eq!(
        super::STRATEGY_ORDER,
        expected,
        "escape_normalized and trimmed_boundary must follow indentation_flexible and precede later strategies"
    );
}

#[test]
fn line_range_for_single_line() {
    let content = "aaa\nbbb\nccc";
    let lr = line_range_for(content, 4, 7);
    assert_eq!(lr.start, 2);
    assert_eq!(lr.end, 2);
}

#[test]
fn line_range_for_multiline() {
    let content = "aaa\nbbb\nccc";
    let lr = line_range_for(content, 4, 11);
    assert_eq!(lr.start, 2);
    assert_eq!(lr.end, 3);
}

// ── New strategy tests: escape_normalized and trimmed_boundary ───────────

#[test]
fn escape_normalized_match_handles_escaped_quotes() {
    // File contains escaped quotes; old_text uses literal quotes. The literal
    // substring does not appear in content, so earlier strategies fail and
    // escape-normalization is reached.
    let content = "let s = \"He said \\\"hello\\\"\";";
    let old_text = "He said \"hello\"";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::EscapeNormalized);
    assert_eq!(m.outcome, MatchOutcome::Success);
    let br = m.byte_range.expect("escape success has byte range");
    assert_eq!(&content[br.start..br.end], "He said \\\"hello\\\"");
}

#[test]
fn escape_normalized_match_handles_escaped_backslashes() {
    // File contains escaped backslashes; old_text uses literal backslashes. The
    // literal substring does not appear in content, so earlier strategies fail and
    // escape-normalization is reached.
    let content = "C:\\\\Users\\foo";
    let old_text = "C:\\Users\\foo";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::EscapeNormalized);
    assert_eq!(m.outcome, MatchOutcome::Success);
    let br = m.byte_range.expect("escape success has byte range");
    assert_eq!(&content[br.start..br.end], content);
}

#[test]
fn escape_normalized_rejects_quote_imbalance_guard() {
    // The candidate crosses a literal boundary because it contains unescaped quotes.
    // old_text uses literal quotes that do not appear in content, so earlier
    // strategies fail and escape-normalization is reached; the guard then rejects it.
    let content = "let a = \"x\"; let b = \\\"x\\\";";
    let old_text = "\"x\"; let b = \"x\"";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::EscapeNormalized);
    assert_eq!(m.outcome, MatchOutcome::GuardRejected);
    assert_eq!(
        m.guard_rejected_reason,
        Some("escape quote balance mismatch")
    );
}

#[test]
fn escape_normalized_rejects_backslash_imbalance_guard() {
    // The candidate starts immediately after an opening backslash escape and
    // ends inside another, so the boundaries split escape sequences.
    // old_text has one fewer backslash than content, so exact fails and
    // escape-normalization is reached.
    let content = "\\x\\\\y";
    let old_text = "x\\y";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::EscapeNormalized);
    assert_eq!(m.outcome, MatchOutcome::GuardRejected);
    assert_eq!(
        m.guard_rejected_reason,
        Some("escape backslash balance mismatch")
    );
}

#[test]
fn escape_normalized_ambiguity_requires_disambiguation() {
    let content = "x\\\" y x\\\" y";
    let old_text = "x\" y";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::EscapeNormalized);
    assert_eq!(m.outcome, MatchOutcome::Ambiguous);
    assert_eq!(m.candidate_count, 2);
}

#[test]
fn trimmed_boundary_ignores_leading_blank_lines() {
    // old_text has a leading whitespace-only boundary line that is not present
    // in content. Exact/line/whitespace/indentation all fail because the
    // leading boundary changes the string, while trimmed_boundary strips it
    // and matches the inner candidate.
    let content = "let x = 1;\nlet y = 2;\n";
    let old_text = "   \nlet x = 1;\nlet y = 2;";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::TrimmedBoundary);
    assert_eq!(m.outcome, MatchOutcome::Success);
    let br = m
        .byte_range
        .expect("trimmed boundary success has byte range");
    assert_eq!(&content[br.start..br.end], "let x = 1;\nlet y = 2;");
}

#[test]
fn trimmed_boundary_ignores_trailing_whitespace_lines() {
    // old_text has a trailing whitespace-only boundary line that is not present
    // in content. Exact/line/whitespace/indentation all fail because the
    // trailing boundary changes the string, while trimmed_boundary strips it.
    let content = "let x = 1;\nlet y = 2;";
    let old_text = "let x = 1;\nlet y = 2;\n   \n";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::TrimmedBoundary);
    assert_eq!(m.outcome, MatchOutcome::Success);
    let br = m
        .byte_range
        .expect("trimmed boundary success has byte range");
    assert_eq!(&content[br.start..br.end], "let x = 1;\nlet y = 2;");
}

#[test]
fn trimmed_boundary_does_not_replace_surrounding_whitespace_lines() {
    // old_text includes extra whitespace-only boundary lines that are not in
    // content, so earlier strategies fail and trimmed_boundary matches the
    // inner candidate once.
    let content = "header\n\nlet x = 1;\nlet y = 2;\n\nfooter\n";
    let old_text = "   \n\n\nlet x = 1;\nlet y = 2;\n\n\n   \n";
    let new_text = "let a = 9;\nlet b = 8;";

    let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs")).unwrap();

    assert_eq!(
        note.as_deref(),
        Some("(matched with trimmed boundary lines)")
    );
    assert!(updated.starts_with("header\n"));
    assert!(updated.contains("\nlet a = 9;\nlet b = 8;\n"));
    assert!(updated.ends_with("footer\n"));
}

#[test]
fn trimmed_boundary_ambiguity_requires_disambiguation() {
    // old_text includes whitespace-only boundary lines that are not in content,
    // so earlier strategies fail. The trimmed inner content appears twice,
    // producing ambiguity under trimmed_boundary.
    let content = "let x = 1;\n\nlet x = 1;";
    let old_text = "   \n\nlet x = 1;\n   \n";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::TrimmedBoundary);
    assert_eq!(m.outcome, MatchOutcome::Ambiguous);
    assert_eq!(m.candidate_count, 2);
}

#[test]
fn first_match_wins_escape_before_trimmed_boundary() {
    // A case that could be matched by both escape_normalized and
    // trimmed_boundary: the earlier escape strategy should win.
    let content = "let s = \"x\\\" y\";";
    let old_text = "x\" y";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::EscapeNormalized);
    assert_eq!(m.outcome, MatchOutcome::Success);
}

#[test]
fn first_match_wins_indentation_before_escape() {
    // A case that indentation_flexible can match; escape_normalized should not
    // shadow it because indentation_flexible is earlier in the chain.
    let content = "    if ready {\n        do_thing();\n    }\n";
    let old_text = "if ready {\n    do_thing();\n}";

    let m = find_match(content, old_text);

    assert_eq!(m.strategy, MatchStrategy::IndentationFlexible);
    assert_eq!(m.outcome, MatchOutcome::Success);
}

// ── Wrapper tests ─────────────────────────────────────────────────────

#[test]
fn wrapper_preserves_exact_match_note_absence() {
    let content = "hello world";
    let (updated, note) =
        fuzzy_replace(content, "world", "universe", Path::new("test.rs")).unwrap();
    assert_eq!(updated, "hello universe");
    assert!(note.is_none(), "exact match should not produce a note");
}

#[test]
fn wrapper_line_trimmed_uses_typed_core() {
    // Multiline: the content's first line has trailing spaces, so the exact
    // two-line substring is NOT present, but line-trimmed matching finds it.
    let content = "let x = 1;   \nlet y = 2;\n";
    let (updated, note) = fuzzy_replace(
        content,
        "let x = 1;\nlet y = 2;",
        "let x = 3;\nlet y = 4;",
        Path::new("test.rs"),
    )
    .unwrap();
    assert_eq!(updated, "let x = 3;\nlet y = 4;\n");
    assert_eq!(
        note.as_deref(),
        Some("(matched after trimming trailing whitespace)")
    );
}

#[test]
fn wrapper_reports_ambiguity_with_strategy_phrase() {
    let content = "foo bar\nfoo bar\n";
    let err = fuzzy_replace(content, "foo", "baz", Path::new("test.rs")).unwrap_err();
    assert!(err.contains("appears 2 times in file"), "got: {err}");
    assert!(err.contains("test.rs"), "got: {err}");
}

#[test]
fn wrapper_reports_no_match() {
    let content = "hello\n";
    let err = fuzzy_replace(content, "missing", "x", Path::new("f.rs")).unwrap_err();
    assert!(err.contains("not found"), "got: {err}");
    assert!(err.contains("f.rs"), "got: {err}");
}

// ── Guard rejection tests ──────────────────────────────────────────────

#[test]
fn guard_rejects_crlf_in_content() {
    // Content uses CRLF line endings. Line-trimmed/whitespace-normalized
    // strategies normalize away \r, but the guard catches the CRLF
    // boundary mismatch when mapping back to original positions.
    let content = "line one\r\nline two\r\n";
    let old_text = "line one\nline two";
    let m = find_match(content, old_text);
    assert_eq!(
        m.outcome,
        MatchOutcome::GuardRejected,
        "expected guard rejection for CRLF content, got {:?}",
        m.outcome
    );
    assert!(m.guard_rejected_reason.is_some());
}

#[test]
fn guard_rejects_partial_line_multiline_match() {
    // Multi-line old_text that starts/ends mid-line in the original.
    let content = "hello world\nfoo bar\n";
    let old_text = "world\nfoo";
    let m = find_match(content, old_text);
    // Exact match finds "world\nfoo" at byte 6..15.
    // Start (6) is mid-line, so the line-boundary guard rejects it.
    assert_eq!(
        m.outcome,
        MatchOutcome::GuardRejected,
        "expected guard rejection for partial-line multi-line match"
    );
    let reason = m
        .guard_rejected_reason
        .expect("guard_rejected_reason must be set");
    assert!(reason.contains("line boundary"), "reason: {reason}");
}

#[test]
fn guard_allows_multiline_at_line_boundaries() {
    let content = "line1\nline2\nline3\n";
    let old_text = "line2\n";
    let m = find_match(content, old_text);
    assert_eq!(m.outcome, MatchOutcome::Success);
    assert!(m.guard_rejected_reason.is_none());
}

#[test]
fn guard_allows_single_line_partial_match() {
    let content = "hello world goodbye\n";
    let old_text = "world";
    let m = find_match(content, old_text);
    assert_eq!(m.outcome, MatchOutcome::Success);
    assert!(m.guard_rejected_reason.is_none());
}

#[test]
fn no_match_nearest_miss_score_reflects_partial_overlap() {
    let content = "function process_data(input) {";
    let old_text = "function process_data(output) {";
    let m = find_match(content, old_text);
    assert_eq!(m.outcome, MatchOutcome::NoMatch);
    let score = m
        .nearest_miss
        .expect("no_match must carry nearest_miss score");
    assert!(score > 0.5, "expected high nearest-miss score, got {score}");
}

#[test]
fn no_match_nearest_miss_zero_for_completely_unrelated() {
    let content = "aaaa\n";
    let old_text = "zzzz";
    let m = find_match(content, old_text);
    assert_eq!(m.outcome, MatchOutcome::NoMatch);
    let score = m
        .nearest_miss
        .expect("no_match must carry nearest_miss score");
    assert_eq!(score, 0.0);
}

#[test]
fn guard_rejection_wrapper_error_is_backward_compatible() {
    let content = "line one\r\nline two\r\n";
    let old_text = "line one\nline two";
    let err = fuzzy_replace(content, old_text, "replacement", Path::new("f.rs")).unwrap_err();
    assert!(
        err.contains("safety guard"),
        "error should mention safety guard: {err}"
    );
    assert!(err.contains("f.rs"), "error should mention path: {err}");
}

#[test]
fn guard_allows_crlf_with_exact_crlf_match() {
    // When both old_text and content use CRLF, exact match works.
    let content = "line one\r\nline two\r\n";
    let old_text = "line one\r\nline two\r\n";
    let m = find_match(content, old_text);
    assert_eq!(m.outcome, MatchOutcome::Success);
    assert!(m.guard_rejected_reason.is_none());
}

#[test]
fn nearest_miss_score_utility() {
    // Exact substring should score 1.0.
    let score = nearest_miss_score("hello world", "world");
    assert_eq!(score, 1.0);
    // No overlap should score 0.0.
    let score = nearest_miss_score("aaaa", "zzzz");
    assert_eq!(score, 0.0);
    // Partial overlap.
    let score = nearest_miss_score("hello world", "hello earth");
    assert!(
        score > 0.4,
        "expected partial overlap score > 0.4, got {score}"
    );
}

#[test]
fn guard_utf8_boundary_allows_valid_ranges() {
    let content = "caf\u{e9} is nice\n";
    let old_text = "caf\u{e9} is nice";
    let m = find_match(content, old_text);
    assert_eq!(m.outcome, MatchOutcome::Success);
    assert!(m.guard_rejected_reason.is_none());
}

#[test]
fn guard_crlf_normalization_rejected_by_all_strategies() {
    let content = "aaa\r\nbbb\r\n";
    let old_text = "aaa\nbbb";
    let m = find_match(content, old_text);
    assert_eq!(m.outcome, MatchOutcome::GuardRejected);
}

#[test]
fn multiline_success_has_byte_and_line_ranges() {
    let content = "line1\nline2\nline3\n";
    let old_text = "line1\nline2\n";
    let m = find_match(content, old_text);
    assert_eq!(m.outcome, MatchOutcome::Success);
    assert!(m.byte_range.is_some(), "success must have byte_range");
    assert!(m.line_range.is_some(), "success must have line_range");
    let lr = m.line_range.unwrap();
    assert_eq!(lr.start, 1);
    assert_eq!(lr.end, 2);
}

#[test]
fn ambiguous_match_still_reports_count_and_no_range() {
    let content = "abc\ndef\nabc\n";
    let old_text = "abc";
    let m = find_match(content, old_text);
    assert_eq!(m.outcome, MatchOutcome::Ambiguous);
    assert_eq!(m.candidate_count, 2);
    assert!(m.byte_range.is_none());
    assert!(m.line_range.is_none());
}
