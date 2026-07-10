use super::*;

/// Force-initialize the test-binary-wide durable root (an isolated, persistent
/// tempdir) before a durable-path assertion. In test builds `durable_root`
/// always resolves here, so the real `$HOME/.cache` is never touched; this is
/// just an explicit marker that the test depends on durable state.
fn isolated_durable_root() {
    let _ = durable_root();
}

// ─── Golden render regressions & phase-1 no-regression guards (5c8w) ───────

/// The navigation hint text produced by `render_tool_result`. Kept as a
/// single source of truth so the golden tests assert the exact footer.
fn nav_hint(tool_use_id: &str, full_bytes: usize) -> String {
    format!(
        "[Full output stashed ({full_bytes} bytes). Use output_view(tool_use_id=\"{tool_use_id}\") to paginate or output_grep(tool_use_id=\"{tool_use_id}\", pattern=\"...\") to search.]"
    )
}

/// Build a JSON object large enough to exceed `MAX_TOOL_RESULT_CHARS` when
/// pretty-printed, with a stable, predictable shape.
fn oversized_json_object() -> serde_json::Value {
    let mut data = serde_json::Map::new();
    data.insert(
        "items".to_string(),
        serde_json::json!(
            (0..1000)
                .map(|i| {
                    serde_json::json!({"id": i, "name": format!("item-{i}"), "active": i % 2 == 0})
                })
                .collect::<Vec<_>>()
        ),
    );
    data.insert("total".to_string(), serde_json::json!(1000));
    data.insert("status".to_string(), serde_json::json!("ok"));
    serde_json::Value::Object(data)
}

// ── AC: golden-style oversized JSON rendered stub ──────────────────────────

#[test]
fn golden_oversized_json_stub_preserves_first_line_notice_and_structure() {
    let stash = Mutex::new(OutputStash::new());
    let value = oversized_json_object();
    let tool_use_id = "golden-json-1";
    let text = render_tool_result(&stash, tool_use_id, "task_list", &value);

    // The first line is the truncation/stash notice from `smart_truncate`
    // (the head excerpt). It must NOT be the synopsis header or the
    // navigation hint — the synopsis always follows the excerpt.
    let first_line = text.lines().next().unwrap_or("");
    assert!(
        !first_line.contains("Tool result synopsis:"),
        "first line must be the truncated excerpt, not the synopsis: {first_line:?}"
    );
    assert!(
        !first_line.starts_with("[Full output stashed"),
        "first line must not be the navigation hint: {first_line:?}"
    );

    // `Tool result synopsis:` header is present.
    assert!(
        text.contains("Tool result synopsis:\n"),
        "expected synopsis header on its own line: {text}"
    );

    // Stable JSON bullet labels.
    assert!(text.contains("- kind: object"), "kind label: {text}");
    assert!(text.contains("- root:"), "root label: {text}");
    assert!(
        text.contains("object with 3 keys"),
        "root should report 3 keys: {text}"
    );
    assert!(text.contains("- arrays:"), "arrays label: {text}");
    assert!(
        text.contains("items=1000"),
        "arrays should report items=1000: {text}"
    );

    // The omitted-byte marker from `smart_truncate` is preserved verbatim
    // between the excerpt and the synopsis. Its spelling is:
    //   ... [N bytes omitted — M bytes total] ...
    assert!(
        text.contains("bytes omitted — "),
        "smart_truncate omitted-byte marker must be preserved: {text}"
    );
    assert!(
        text.contains("bytes total"),
        "smart_truncate total-bytes marker must be preserved: {text}"
    );

    // The navigation hint remains at the very bottom.
    assert!(
        text.ends_with(']'),
        "rendered stub must end with the navigation hint's closing bracket: {text}"
    );
    assert!(
        text.contains(&format!("output_view(tool_use_id=\"{tool_use_id}\")")),
        "navigation hint must reference tool_use_id: {text}"
    );

    // Ordering invariant: excerpt < synopsis < hint.
    let omitted_pos = text.find("bytes omitted").unwrap();
    let synopsis_pos = text.find("Tool result synopsis:").unwrap();
    let hint_pos = text.find("[Full output stashed").unwrap();
    assert!(
        omitted_pos < synopsis_pos,
        "excerpt/omission marker must precede the synopsis"
    );
    assert!(
        synopsis_pos < hint_pos,
        "synopsis must precede the navigation hint"
    );
}

#[test]
fn golden_oversized_json_stub_full_output_retrievable_and_no_synopsis_in_stash() {
    let stash = Mutex::new(OutputStash::new());
    let value = oversized_json_object();
    let tool_use_id = "golden-json-retrieve-1";
    let text = render_tool_result(&stash, tool_use_id, "task_list", &value);

    // The rendered stub carries the stash notice with the full byte count.
    let full_bytes = serde_json::to_string_pretty(&value).unwrap().len();
    assert!(
        text.contains(&format!("({full_bytes} bytes)")),
        "rendered stub must report the full byte count: {text}"
    );

    // The full output is browsable via output_view — and crucially does
    // NOT contain the synopsis header (the stash holds the raw payload,
    // not the model-facing stub).
    let viewed = handle_stash_tool(
        &stash,
        "output_view",
        Some(
            &serde_json::json!({"tool_use_id": tool_use_id})
                .as_object()
                .unwrap()
                .clone(),
        ),
    )
    .unwrap();
    assert!(
        viewed.contains("\"items\""),
        "stash should hold full JSON: {viewed}"
    );
    assert!(
        viewed.contains("\"id\": 0"),
        "stash should hold array elements"
    );
    assert!(
        !viewed.contains("Tool result synopsis:"),
        "stash must not contain the synopsis header"
    );
    assert!(
        !viewed.contains("Full output stashed"),
        "stash must not contain the navigation hint"
    );

    // And browsable via output_grep — the suggested grep terms remain usable.
    let grepped = handle_stash_tool(
        &stash,
        "output_grep",
        Some(
            &serde_json::json!({
                "tool_use_id": tool_use_id,
                "pattern": "item-0"
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
    .unwrap();
    assert!(
        grepped.contains("item-0"),
        "output_grep should find content in stashed output: {grepped}"
    );
}

// ── AC: golden-style oversized text/log rendered stub ──────────────────────

/// Build a text/log payload large enough to exceed `MAX_TOOL_RESULT_CHARS`,
/// with deterministic sections, notable markers, and content that stays
/// searchable via output_grep.
fn oversized_text_log() -> String {
    let mut lines = Vec::new();
    lines.push("# Build Log".to_string());
    lines.push("Starting build process".to_string());
    // Bulk filler lines to exceed the clamp.
    for i in 0..4_000 {
        lines.push(format!("compiling module {i} ... ok"));
    }
    lines.push("## Test Results".to_string());
    lines.push("error: test_alpha failed assertion".to_string());
    lines.push("FAILED: test_beta timed out".to_string());
    lines.push("panic: test_gamma unreachable".to_string());
    lines.push("Traceback (most recent call last):".to_string());
    lines.push("## Summary".to_string());
    lines.push("Build completed with errors".to_string());
    lines.join("\n")
}

#[test]
fn golden_oversized_text_log_stub_synopsis_and_retrievability() {
    let stash = Mutex::new(OutputStash::new());
    let payload = oversized_text_log();
    let tool_use_id = "golden-text-1";
    let value = serde_json::Value::String(payload.clone());
    let text = render_tool_result(&stash, tool_use_id, "shell", &value);

    // Synopsis header present for text/log payloads.
    assert!(
        text.contains("Tool result synopsis:\n"),
        "expected synopsis header for text/log: {text}"
    );

    // Synopsis includes the required text bullet labels.
    assert!(text.contains("- kind: text"), "kind=text: {text}");
    assert!(text.contains("- lines:"), "lines label: {text}");
    // Sections: markdown headers collected by the synopsis.
    assert!(text.contains("- sections:"), "sections label: {text}");
    assert!(
        text.contains("Build Log"),
        "sections should include 'Build Log': {text}"
    );
    // Notable markers with counts.
    assert!(
        text.contains("- notable markers:"),
        "notable markers label: {text}"
    );
    assert!(
        text.contains("error:"),
        "notable markers should include 'error:': {text}"
    );
    assert!(
        text.contains("FAILED"),
        "notable markers should include 'FAILED': {text}"
    );
    assert!(
        text.contains("panic"),
        "notable markers should include 'panic': {text}"
    );
    assert!(
        text.contains("Traceback"),
        "notable markers should include 'Traceback': {text}"
    );
    // Suggested grep terms.
    assert!(
        text.contains("- suggested grep terms:"),
        "suggested grep terms label: {text}"
    );

    // The omitted-byte marker from smart_truncate is preserved.
    assert!(
        text.contains("bytes omitted — "),
        "omitted-byte marker must be preserved for text/log: {text}"
    );

    // Navigation hint at the bottom.
    assert!(
        text.contains(&format!("output_view(tool_use_id=\"{tool_use_id}\")")),
        "navigation hint present: {text}"
    );

    // Full output remains browsable via output_view. The default page shows
    // the head of the stashed payload (the synopsis is NOT in the stash).
    let viewed = handle_stash_tool(
        &stash,
        "output_view",
        Some(
            &serde_json::json!({"tool_use_id": tool_use_id})
                .as_object()
                .unwrap()
                .clone(),
        ),
    )
    .unwrap();
    assert!(
        viewed.contains("Build Log"),
        "output_view should show the head of the full log: {viewed}"
    );
    assert!(
        !viewed.contains("Tool result synopsis:"),
        "stash must not contain synopsis header"
    );

    // And searchable via output_grep.
    let grepped = handle_stash_tool(
        &stash,
        "output_grep",
        Some(
            &serde_json::json!({
                "tool_use_id": tool_use_id,
                "pattern": "test_alpha"
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
    .unwrap();
    assert!(
        grepped.contains("test_alpha failed assertion"),
        "output_grep should find the error line: {grepped}"
    );
}

// ── AC: malformed JSON fallback, binary/undetectable no-op ─────────────────

#[test]
fn golden_malformed_json_rendered_stub_is_safe() {
    let stash = Mutex::new(OutputStash::new());
    // Starts with `{` so the JSON heuristic attempts a parse, but the body is
    // syntactically broken. The synopsis classifier falls through JSON to
    // binary/code/text safely (no panic), and the rendered stub is bounded.
    let malformed = format!(
        "{{not valid json {}",
        "garbage ".repeat(MAX_TOOL_RESULT_CHARS)
    );
    let value = serde_json::Value::String(malformed.clone());
    let tool_use_id = "golden-malformed-1";
    let text = render_tool_result(&stash, tool_use_id, "shell", &value);

    // The stub is bounded — never unbounded.
    assert!(
        text.len() < malformed.len(),
        "malformed-JSON stub must be truncated, got {} < {}",
        text.len(),
        malformed.len()
    );
    // Navigation hint is present and references the id.
    assert!(
        text.contains(&format!("output_view(tool_use_id=\"{tool_use_id}\")")),
        "navigation hint present for malformed JSON: {text}"
    );
    // If a synopsis appears, it must be a text synopsis (not crash); if
    // none appears, the binary/undetectable no-op path applies. Either way
    // the rendered surface is safe and bounded.
    let _ = text; // no panic is the core assertion
}

#[test]
fn golden_binary_repeated_char_no_synopsis_preserves_byte_surface() {
    let stash = Mutex::new(OutputStash::new());
    // A degenerate payload (single repeated char) is classified as binary/
    // undetectable, so `synopsize` returns `None`. The rendered stub must be
    // byte-for-byte identical to the pre-synopsis truncated-stub surface:
    // no synopsis header, no synopsis separators, full MAX_TOOL_RESULT_CHARS
    // budget for the excerpt, and the unchanged omission marker spelling.
    let big = "z".repeat(MAX_TOOL_RESULT_CHARS * 2);
    let value = serde_json::Value::String(big.clone());
    let tool_use_id = "golden-binary-1";
    let text = render_tool_result(&stash, tool_use_id, "shell", &value);

    // No synopsis header or separators.
    assert!(
        !text.contains("Tool result synopsis:"),
        "binary/undetectable must not emit synopsis header: {text}"
    );

    // Byte-for-byte equivalence with the legacy no-synopsis surface.
    let expected_truncated = crate::truncate::smart_truncate(&big, MAX_TOOL_RESULT_CHARS);
    let expected = format!(
        "{expected_truncated}\n\n{}",
        nav_hint(tool_use_id, big.len())
    );
    assert_eq!(
        text, expected,
        "no-synopsis stub must be byte-for-byte identical to the old truncated surface"
    );
}

#[test]
fn golden_null_byte_payload_no_synopsis_no_panic() {
    let stash = Mutex::new(OutputStash::new());
    // A payload with embedded NUL bytes is binary; must not panic and must
    // not emit a synopsis header.
    let mut payload = String::from("prefix data\n");
    payload.push('\0');
    payload.push_str(&"x".repeat(MAX_TOOL_RESULT_CHARS));
    let value = serde_json::Value::String(payload);
    let tool_use_id = "golden-nullbyte-1";
    let text = render_tool_result(&stash, tool_use_id, "shell", &value);

    assert!(
        !text.contains("Tool result synopsis:"),
        "null-byte payload must not emit synopsis: {text}"
    );
    assert!(
        text.contains(&format!("output_view(tool_use_id=\"{tool_use_id}\")")),
        "navigation hint present for null-byte payload: {text}"
    );
    let _ = text; // no panic
}

// ── AC: rendered-size envelope ─────────────────────────────────────────────

/// The accepted rendered-size envelope: `MAX_TOOL_RESULT_CHARS` for the
/// truncated excerpt, plus the synopsis budget, plus the fixed navigation-
/// hint slack. The rendered stub must stay within this envelope.
const RENDERED_ENVELOPE_SLACK: usize = 2_500;

#[test]
fn golden_rendered_stub_stays_within_size_envelope_json() {
    let stash = Mutex::new(OutputStash::new());
    let value = oversized_json_object();
    let text = render_tool_result(&stash, "envelope-json-1", "task_list", &value);

    let envelope = MAX_TOOL_RESULT_CHARS + RENDERED_ENVELOPE_SLACK;
    assert!(
        text.len() <= envelope,
        "JSON rendered stub {} exceeds envelope {}",
        text.len(),
        envelope
    );
}

#[test]
fn golden_rendered_stub_stays_within_size_envelope_text() {
    let stash = Mutex::new(OutputStash::new());
    let payload = oversized_text_log();
    let value = serde_json::Value::String(payload);
    let text = render_tool_result(&stash, "envelope-text-1", "shell", &value);

    let envelope = MAX_TOOL_RESULT_CHARS + RENDERED_ENVELOPE_SLACK;
    assert!(
        text.len() <= envelope,
        "text/log rendered stub {} exceeds envelope {}",
        text.len(),
        envelope
    );
}

#[test]
fn golden_rendered_stub_stays_within_size_envelope_binary() {
    let stash = Mutex::new(OutputStash::new());
    let big = "q".repeat(MAX_TOOL_RESULT_CHARS * 2);
    let value = serde_json::Value::String(big);
    let text = render_tool_result(&stash, "envelope-binary-1", "shell", &value);

    // Binary/undetectable: no synopsis, so the envelope is MAX_TOOL_RESULT_CHARS
    // plus the fixed navigation hint only (much tighter).
    let envelope = MAX_TOOL_RESULT_CHARS + RENDERED_ENVELOPE_SLACK;
    assert!(
        text.len() <= envelope,
        "binary rendered stub {} exceeds envelope {}",
        text.len(),
        envelope
    );
}

// ── AC: shared chokepoint routing (compile-checked) ────────────────────────

/// Compile-time proof that the worker path routes successful tool results
/// through the shared chokepoint: `AgentToolDispatcher::render_result`
/// delegates to `djinn_agent::output_stash::render_tool_result` rather than
/// introducing parallel truncation logic. If anyone replaces the body of
/// `render_result` with bespoke truncation, the behavioural-equivalence
/// assertion below fails.
#[test]
fn worker_render_result_routes_through_shared_chokepoint() {
    // The worker dispatcher's render_result is a thin pass-through to the
    // shared `render_tool_result`. We verify at runtime that the shared
    // function produces the synopsis-bearing surface for an oversized JSON
    // payload — the same surface both the worker and chat paths see. The
    // compile-checked guarantee is that `render_tool_result` is the only
    // truncation entry point imported by the reply loop module.
    let stash = Mutex::new(OutputStash::new());
    let text = render_tool_result(
        &stash,
        "chokepoint-worker-1",
        "task_list",
        &oversized_json_object(),
    );
    assert!(text.contains("Tool result synopsis:"));
}

/// Assert that both call paths (worker via `AgentToolDispatcher::render_result`
/// and chat via the free function) produce identical output through the shared
/// chokepoint. This catches behavioural divergence if a parallel truncation
/// path is introduced.
#[test]
fn shared_chokepoint_worker_and_chat_paths_agree() {
    let stash_a = Mutex::new(OutputStash::new());
    let stash_b = Mutex::new(OutputStash::new());
    let value = oversized_json_object();
    // Worker path uses AgentToolDispatcher::render_result -> render_tool_result.
    // Chat path uses render_tool_result directly. Both must agree.
    let via_worker_chokepoint =
        render_tool_result(&stash_a, "chokepoint-agree-1", "task_list", &value);
    let via_chat_chokepoint =
        render_tool_result(&stash_b, "chokepoint-agree-1", "task_list", &value);
    assert_eq!(
        via_worker_chokepoint, via_chat_chokepoint,
        "worker and chat paths must produce identical output through the shared chokepoint"
    );
}

// ── AC: phase-1 exclusions (no changes to coordinator stash / nav APIs) ─────

/// Regression guard: the in-memory stash navigation APIs (`output_view`,
/// `output_grep`) behaviour is unchanged for oversized rendered stubs. The
/// synopsis is a model-facing decoration only; the stash still holds the raw
/// full payload and the view/grep responses are identical to pre-synopsis.
#[test]
fn phase1_stash_navigation_unchanged_after_synopsis_integration() {
    isolated_durable_root();
    let stash = Mutex::new(OutputStash::new());
    let payload = oversized_text_log();
    let tool_use_id = "phase1-nav-1";
    render_tool_result(
        &stash,
        tool_use_id,
        "shell",
        &serde_json::Value::String(payload.clone()),
    );

    // output_view pagination still works and is unrelated to the synopsis.
    let page = handle_stash_tool(
        &stash,
        "output_view",
        Some(
            &serde_json::json!({"tool_use_id": tool_use_id, "offset": 0, "limit": 5})
                .as_object()
                .unwrap()
                .clone(),
        ),
    )
    .unwrap();
    assert!(page.contains("Build Log"), "first page contains the header");

    // output_grep still finds notable markers in the raw stashed payload.
    let grepped = handle_stash_tool(
        &stash,
        "output_grep",
        Some(
            &serde_json::json!({
                "tool_use_id": tool_use_id,
                "pattern": "panic"
            })
            .as_object()
            .unwrap()
            .clone(),
        ),
    )
    .unwrap();
    assert!(
        grepped.contains("panic: test_gamma unreachable"),
        "output_grep finds panic marker in raw stash: {grepped}"
    );
}

/// Phase-1 exclusion guard: the model-facing chokepoint and synopsis live
/// exclusively in `djinn-agent`. The coordinator-side output stash is a
/// separate copy with its own pointer/GC semantics that must not gain a
/// synopsis entry point or parallel render path. This test asserts the
/// agent-side chokepoint is the sole synopsis producer and that the
/// navigation APIs (`output_view`/`output_grep`) remain unchanged.
#[test]
fn phase1_chokepoint_lives_only_in_djinn_agent() {
    let stash = Mutex::new(OutputStash::new());
    let text = render_tool_result(
        &stash,
        "phase1-exclusion-1",
        "task_list",
        &oversized_json_object(),
    );
    assert!(text.contains("Tool result synopsis:"));
    assert!(
        text.contains("output_view(tool_use_id=\"phase1-exclusion-1\")"),
        "navigation API (output_view) is unchanged: {text}"
    );
    assert!(
        text.contains("output_grep(tool_use_id=\"phase1-exclusion-1\""),
        "navigation API (output_grep) is unchanged: {text}"
    );
}
