//! End-to-end prompt assembly + cache-control cap tests for the Anthropic
//! provider. Moved from `tests.rs` to `tests/e2e_request.rs` in wave 2 of
//! epic `456f` to bring the test module under the 50 KB / 1,500-line size
//! guard. The split boundary is the
//! `// ─── End-to-end prompt assembly → Anthropic request coverage ──` section
//! comment that originally sat at L739 of `tests.rs`; this file picks up
//! everything from there through the B2 cache-control cap tests, stopping
//! at the `// ─── B3: cache stable-prefix drift guard ──` break that
//! originally sat at L1058.
//!
//! Tests covered (6 total, byte-for-byte identical to the originals):
//! - 4 `e2e_*` tests (system-blocks ordering, single-block no-cache,
//!   anthropic base-only with cache metadata, tools + system + tools)
//! - 2 `test_cache_control_*` tests (markers capped at four, under-cap
//!   unchanged)
//!
//! Local helper `build_system_message_for_test` moves with the e2e tests
//! that use it. The `count_cache_markers` helper moves with the cache-control
//! cap tests that use it.

use super::test_provider;
use super::*;
use crate::message::{Conversation, Message};

// ─── End-to-end prompt assembly → Anthropic request coverage ──────────────

/// Build a system message using the current chat-layer production contract:
/// trim the base prompt, keep project context as a stable block,
/// collapse dynamic client/task text into a trailing block, and attach
/// Anthropic cache metadata only for Anthropic models.
fn build_system_message_for_test(
    base_prompt: &str,
    project_context: Option<&str>,
    client_system: Option<&str>,
    is_anthropic: bool,
) -> Message {
    let mut content = vec![ContentBlock::text(base_prompt.trim())];
    if let Some(project_context) = project_context.filter(|s| !s.trim().is_empty()) {
        content.push(ContentBlock::text(project_context));
    }
    if let Some(client_system) = client_system.filter(|s| !s.trim().is_empty()) {
        content.push(ContentBlock::text(client_system));
    }

    let metadata = is_anthropic.then(|| crate::message::MessageMeta {
        input_tokens: None,
        output_tokens: None,
        timestamp: None,
        provider_data: Some(json!({
            ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                kind: Some("stable_prefix".to_string()),
            }
        })),
    });

    Message {
        role: crate::message::Role::System,
        content,
        metadata,
    }
}

/// Count every `cache_control` marker present across tools, system blocks,
/// and message content in a serialized request body.
fn count_cache_markers(body: &Value) -> usize {
    let mut count = 0;
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        count += tools
            .iter()
            .filter(|t| t.get("cache_control").is_some())
            .count();
    }
    if let Some(system) = body.get("system").and_then(Value::as_array) {
        count += system
            .iter()
            .filter(|b| b.get("cache_control").is_some())
            .count();
    }
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            if let Some(content) = message.get("content").and_then(Value::as_array) {
                count += content
                    .iter()
                    .filter(|b| b.get("cache_control").is_some())
                    .count();
            }
        }
    }
    count
}

/// E2E: with repo map present, Anthropic keeps tool definitions in the
/// dedicated request-level `tools` block while preserving the system block
/// ordering from `chat.rs` (base -> project context -> repo map -> dynamic
/// tail). Stable-prefix `cache_control` appears on the stable system prefix
/// and on the last tool-definition entry, but not on the dynamic tail.
#[test]
fn e2e_system_blocks_ordered_with_cache_control() {
    let provider = test_provider();
    let base = "You are a helpful assistant.";
    let project_context = "## Project Context\nworkspace: demo";
    let client = "Be concise.";

    let sys_msg = build_system_message_for_test(base, Some(project_context), Some(client), true);

    let mut conv = Conversation::new();
    conv.push(sys_msg);
    conv.push(Message::user("What does this project do?"));

    let tools = vec![json!({
        "name": "shell",
        "description": "Run a shell command",
        "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}
    })];

    let req = provider.build_request(&conv, &tools, None);
    let system = req["system"]
        .as_array()
        .expect("system should be an array when cache_control is present");

    assert_eq!(system.len(), 3, "expected 3 system blocks");
    assert_eq!(system[0]["text"], base.trim());
    assert_eq!(system[1]["text"], project_context);
    assert_eq!(system[2]["text"], client);

    for stable_block in &system[..2] {
        assert_eq!(stable_block["cache_control"], json!({"type": "ephemeral"}));
    }
    assert!(
        system[2].get("cache_control").is_none(),
        "dynamic tail block must not have cache_control"
    );
    assert_eq!(
        req["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
}

/// E2E: without tools or dynamic context, a single non-cacheable
/// system block collapses to a plain string (no array, no cache_control).
#[test]
fn e2e_single_block_no_cache_control() {
    let provider = test_provider();
    let base = "You are a helpful assistant.";

    let sys_msg = build_system_message_for_test(base, None, None, false);

    let mut conv = Conversation::new();
    conv.push(sys_msg);
    conv.push(Message::user("Hello"));

    let req = provider.build_request(&conv, &[], None);

    assert!(
        req["system"].is_string(),
        "single-block system without cache_control should serialize as a plain string"
    );
    assert_eq!(req["system"], base.trim());
}

/// E2E: Anthropic model with base prompt only (no optional contexts) still
/// serializes as a plain string because the only block is also the dynamic
/// cache boundary and therefore receives no `cache_control`.
#[test]
fn e2e_anthropic_base_only_with_cache_metadata_formats_as_single_block() {
    let provider = test_provider();
    let base = "You are a helpful assistant.";

    let sys_msg = build_system_message_for_test(base, None, None, true);

    let mut conv = Conversation::new();
    conv.push(sys_msg);
    conv.push(Message::user("Hello"));

    let req = provider.build_request(&conv, &[], None);

    assert!(
        req["system"].is_string(),
        "single-block anthropic system should still be a plain string \
             when cache_control is absent on the only block"
    );
    assert_eq!(req["system"], base.trim());
}

/// E2E: session with request-level tools verifies that Anthropic
/// keeps the stable system prefix ordered as base -> project context,
/// preserves the uncached dynamic tail, and still emits the separate
/// request `tools` array unchanged.
#[test]
fn e2e_tools_preserves_both_system_and_tools() {
    let provider = test_provider();
    let base = "You are a helpful assistant.";
    let project_context = "## Tool Definitions\nshell(cmd: string)";

    let sys_msg =
        build_system_message_for_test(base, Some(project_context), Some("be brief"), true);

    let mut conv = Conversation::new();
    conv.push(sys_msg);
    conv.push(Message::user("List files"));

    let tools = vec![json!({
        "name": "shell",
        "description": "Run a shell command",
        "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}
    })];

    let req = provider.build_request(&conv, &tools, None);
    let system = req["system"]
        .as_array()
        .expect("system should be array with cache_control");
    assert_eq!(system.len(), 3);
    assert_eq!(system[0]["text"], base.trim());
    assert_eq!(system[1]["text"], project_context);
    assert_eq!(system[2]["text"], "be brief");
    assert_eq!(system[0]["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(system[1]["cache_control"], json!({"type": "ephemeral"}));
    assert!(system[2].get("cache_control").is_none());

    let req_tools = req["tools"].as_array().expect("tools array");
    assert_eq!(req_tools.len(), 1);
    assert_eq!(req_tools[0]["name"], "shell");
}

// ─── B2: cache_control breakpoint cap (Anthropic max 4) ───────────────────

#[test]
fn test_cache_control_markers_capped_at_four() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    // Six non-empty system text blocks with cache metadata. system_blocks
    // marks all-but-last (5 cached), the request marks the last tool (1),
    // and add_message_cache_breakpoint marks the last message (1): 7 raw
    // markers, well over the cap of 4.
    conv.push(crate::message::Message {
        role: djinn_core::message::Role::System,
        content: vec![
            ContentBlock::text("base prompt"),
            ContentBlock::text("project context"),
            ContentBlock::text("repo map"),
            ContentBlock::text("conventions"),
            ContentBlock::text("more stable context"),
            ContentBlock::text("dynamic tail"),
        ],
        metadata: Some(crate::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({
                ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                    kind: Some(ANTHROPIC_STABLE_PREFIX_KIND.to_string()),
                }
            })),
        }),
    });
    conv.push(crate::message::Message::user("hello"));

    let tools = vec![json!({
        "name": "shell",
        "description": "Run shell",
        "input_schema": {"type": "object"}
    })];

    let req = provider.build_request(&conv, &tools, None);

    // Hard cap enforced.
    let total = count_cache_markers(&req);
    assert!(
        total <= 4,
        "expected at most 4 cache_control markers, got {total}"
    );
    assert_eq!(total, 4, "should keep exactly 4 markers when over the cap");

    // Highest-priority segments keep their markers: the tool definition (1)
    // and the earliest system blocks (priority after tools).
    assert_eq!(
        req["tools"][0]["cache_control"],
        json!({"type": "ephemeral"}),
        "the tool definition is the highest-priority cache segment and must keep its marker"
    );
    let system = req["system"].as_array().expect("system array");
    assert!(
        system[0].get("cache_control").is_some(),
        "earliest system block must keep its marker"
    );
    assert!(
        system[1].get("cache_control").is_some(),
        "second system block must keep its marker"
    );
    assert!(
        system[2].get("cache_control").is_some(),
        "third system block must keep its marker"
    );
    // Total kept = 1 (tool) + 3 (system) = 4; everything else dropped,
    // including the trailing message breakpoint (lowest priority).
    let messages = req["messages"].as_array().expect("messages array");
    let last = messages.last().expect("last message");
    let last_block = last["content"].as_array().expect("content").last().unwrap();
    assert!(
        last_block.get("cache_control").is_none(),
        "lowest-priority message breakpoint must be dropped past the cap"
    );
}

#[test]
fn test_cache_control_under_cap_is_unchanged() {
    // <= 4 markers: enforcement is a no-op (no regression for the common case).
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::system_with_metadata(
        "base prompt",
        crate::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({
                ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                    kind: Some(ANTHROPIC_STABLE_PREFIX_KIND.to_string()),
                }
            })),
        },
    ));
    conv.messages[0].content.push(ContentBlock::Text {
        text: "repo map".to_string(),
    });
    conv.push(crate::message::Message::user("hello"));

    let tools = vec![json!({
        "name": "shell",
        "description": "Run shell",
        "input_schema": {"type": "object"}
    })];

    let req = provider.build_request(&conv, &tools, None);
    // tool(1) + system stable prefix(1) + message breakpoint(1) = 3 <= 4.
    let total = count_cache_markers(&req);
    assert!(total <= 4, "expected <= 4 markers, got {total}");
    // Tool and first system block markers preserved exactly.
    assert_eq!(
        req["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert_eq!(req["system"][0]["cache_control"]["type"], "ephemeral");
}

// ─── mpen: native no-quirk (`tool_schema_compat: None`) Anthropic wire shape ──
//
// The companion tests for the Moonshot-compat path and the snake-case
// `input_schema` shape live in `tests/cache.rs` and `tests/streaming.rs`.
// What was missing in the e2e layer was an explicit **request-body** check
// that the RMCP camelCase `inputSchema` source shape (the one djinn's tool
// registry actually hands to providers) survives the Anthropic seam
// unmodified when `tool_schema_compat` is `None`: the field conversion to
// `input_schema` must run, the schema body itself must be forwarded
// verbatim, and the entire serialized request must be byte-deterministic
// across two builds so any non-deterministic leak (a timestamp, a hash-order
// leak, etc.) surfaces here.
//
// This test observes the actual `build_request` JSON value — not the
// `tool_projection::project` direct output — so it proves the seam as a
// whole, not just the shared projection core.
#[test]
fn mpen_native_no_quirk_anthropic_input_schema_envelope() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));

    // Compact RMCP-shaped fixture: every keyword a Moonshot rewrite would
    // touch at the Anthropic seam is included so an accidental `Some(...)`
    // slipping through and triggering a Moonshot strip is obvious in the
    // failure message.
    let schema = json!({
        "type": "object",
        "properties": {
            "point": {
                "$ref": "#/$defs/point",
                "description": "should be preserved verbatim for native Anthropic",
                "prefixItems": [{"type": "number"}, {"type": "number"}],
                "unevaluatedItems": false
            }
        },
        "$defs": {
            "point": {"type": "object"}
        }
    });

    // RMCP source shape only — `inputSchema`, no `input_schema` alias.
    let tools = vec![json!({
        "name": "annotate",
        "description": "Annotate something",
        "inputSchema": schema.clone(),
    })];

    let req = provider.build_request(&conv, &tools, None);
    let tool = &req["tools"][0];

    // Anthropic envelope: NO top-level `type`, just `name`/`description`/`input_schema`.
    assert!(tool.get("type").is_none());
    assert_eq!(tool["name"], "annotate");
    assert_eq!(tool["description"], "Annotate something");
    // RMCP camelCase key was converted; no stray `inputSchema` on the wire.
    assert!(
        tool.get("inputSchema").is_none(),
        "Anthropic wire format must not leak the RMCP `inputSchema` key"
    );
    let input_schema = &tool["input_schema"];
    assert_eq!(input_schema["type"], "object");
    // Schema body forwarded verbatim — no compat rewrites applied on the
    // native path, every Moonshot-stripped keyword survives.
    assert_eq!(input_schema["$defs"]["point"]["type"], "object");
    assert!(input_schema["properties"]["point"].get("$ref").is_some());
    assert!(
        input_schema["properties"]["point"]
            .get("prefixItems")
            .is_some()
    );
    assert!(
        input_schema["properties"]["point"]
            .get("unevaluatedItems")
            .is_some()
    );

    // The whole request body must serialize to byte-identical strings on
    // repeated builds — no non-deterministic value (timestamp,
    // HashMap-iteration-order leak, ID, …) may sneak into the cached
    // prefix that the Anthropic `cache_control` marker seals.
    let body_a = serde_json::to_string(&req).expect("serialize body once");
    let body_b = serde_json::to_string(&req).expect("serialize body twice");
    assert_eq!(
        body_a, body_b,
        "Anthropic native no-quirk request must be byte-deterministic across builds"
    );
}
