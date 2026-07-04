//! stable-prefix-hash + default-cache-policy + effective_url + RMCP tests
//! for the Anthropic provider. Moved from `tests.rs` to `tests/cache.rs`
//! in wave 2 of epic `456f` to bring the test module under the
//! 50 KB / 1,500-line size guard. The split boundary is the
//! `// ─── B3: cache stable-prefix drift guard ──` section comment that
//! originally sat at L1058 of `tests.rs`; this file picks up everything
//! from there to the end of the file.
//!
//! Tests covered (13 total, byte-for-byte identical to the originals):
//! - 7 `test_stable_prefix_hash_*` tests (deterministic, perturbed,
//!   dynamic-tail-ignored, growing-conversation, system/tool mutation,
//!   key-order-independent, none-when-no-markers)
//! - 3 `test_default_cache_policy_*` / `test_explicit_metadata_*` tests
//!   (marks-tools-system-and-trailing-message, inactive-without-tools,
//!   explicit-metadata-overrides-default-policy)
//! - 1 `test_effective_url_joins_native_and_v1_suffixed_bases` test
//! - 2 `test_rmcp_tools_*` / `test_tool_without_schema_*` tests
//!   (rmcp-converted-to-anthropic-input-schema, default-input-schema)
//!
//! Local helper `drift_guard_fixture` moves with the stable-prefix-hash
//! tests that use it.

use super::*;
use super::{test_anthropic_config, test_provider};
use crate::message::Conversation;

// ─── B3: cache stable-prefix drift guard ──────────────────────────────────

/// Build a representative cache-enabled conversation + tools used by the B3
/// drift-guard tests: a stable base prompt, a stable project-context block, a
/// dynamic trailing block, plus a tool definition. Mirrors the production
/// chat-layer contract so the cache_control markers land on tools + system
/// prefix + trailing message breakpoint.
fn drift_guard_fixture() -> (Conversation, Vec<Value>) {
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::system_with_metadata(
        "base prompt",
        crate::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({
                ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                    kind: Some("stable_prefix".to_string()),
                }
            })),
        },
    ));
    conv.messages[0].content.push(ContentBlock::Text {
        text: "project context / repo map".to_string(),
    });
    conv.messages[0].content.push(ContentBlock::Text {
        text: "dynamic tail".to_string(),
    });
    conv.push(crate::message::Message::user("hello"));

    let tools = vec![json!({
        "name": "shell",
        "description": "Run shell",
        "input_schema": {"type": "object"}
    })];
    (conv, tools)
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

/// Determinism: identical logical inputs must produce a byte-identical cached
/// prefix, hence an identical stable-prefix hash, across two independent
/// builds. This is the core invariant prompt caching depends on — if it ever
/// fails, some non-deterministic value (timestamp, map-iteration order, …) has
/// leaked into the cached prefix and every cache hit silently becomes a miss.
#[test]
fn test_stable_prefix_hash_is_deterministic() {
    let provider = test_provider();
    let (conv, tools) = drift_guard_fixture();

    let body_a = provider.build_request(&conv, &tools, None);
    let body_b = provider.build_request(&conv, &tools, None);

    let hash_a = AnthropicProvider::stable_prefix_hash(&body_a)
        .expect("cache markers present => hash should exist");
    let hash_b = AnthropicProvider::stable_prefix_hash(&body_b)
        .expect("cache markers present => hash should exist");

    assert_eq!(
        hash_a, hash_b,
        "stable cache prefix must hash identically across two builds of the same inputs"
    );
    // And the full serialized prefix bytes must match too (stronger than the
    // hash, and guards against an accidental hash collision masking real drift).
    assert_eq!(
        serde_json::to_string(&body_a["system"]).unwrap(),
        serde_json::to_string(&body_b["system"]).unwrap(),
        "serialized system prefix must be byte-identical"
    );
    assert_eq!(
        serde_json::to_string(&body_a["tools"]).unwrap(),
        serde_json::to_string(&body_b["tools"]).unwrap(),
        "serialized tool prefix must be byte-identical"
    );
}

/// The hash must be sensitive to actual changes in the cached prefix: a
/// perturbed stable block (here, a mutated project-context string) must yield a
/// different hash. Without this, the guard would never detect real drift.
#[test]
fn test_stable_prefix_hash_detects_perturbed_prefix() {
    let provider = test_provider();
    let (conv, tools) = drift_guard_fixture();
    let baseline = provider.build_request(&conv, &tools, None);
    let baseline_hash = AnthropicProvider::stable_prefix_hash(&baseline).unwrap();

    // Perturb a STABLE (cache_control-marked) system block, simulating a
    // timestamp / non-deterministic leak into the supposedly-stable prefix.
    let mut perturbed = conv.clone();
    perturbed.messages[0].content[1] = ContentBlock::Text {
        text: "project context / repo map @ 2026-06-01T12:00:00Z".to_string(),
    };
    let perturbed_body = provider.build_request(&perturbed, &tools, None);
    let perturbed_hash = AnthropicProvider::stable_prefix_hash(&perturbed_body).unwrap();

    assert_ne!(
        baseline_hash, perturbed_hash,
        "a mutated stable-prefix block must change the stable-prefix hash"
    );
}

/// A change confined to the DYNAMIC tail (the trailing message after the
/// breakpoint) must NOT change the stable-prefix hash — otherwise the guard
/// would warn on every legitimately-changing turn and become noise. Here the
/// trailing breakpoint marker sits on the last *user* message content; the
/// stable prefix is tools + system blocks, which are unchanged.
#[test]
fn test_stable_prefix_hash_ignores_dynamic_tail_changes() {
    let provider = test_provider();
    let (conv, tools) = drift_guard_fixture();
    let body_a = provider.build_request(&conv, &tools, None);

    let mut conv_b = conv.clone();
    // The last message is the user turn; its content is the dynamic tail and is
    // not part of the cached system/tool prefix.
    conv_b.push(crate::message::Message::user(
        "a different follow-up question",
    ));
    let body_b = provider.build_request(&conv_b, &tools, None);

    // The system + tool stable prefix is byte-identical, so those serialized
    // segments must match even though the conversation tail differs.
    assert_eq!(
        serde_json::to_string(&body_a["system"]).unwrap(),
        serde_json::to_string(&body_b["system"]).unwrap(),
        "system prefix must be unaffected by a dynamic-tail change"
    );
    assert_eq!(
        serde_json::to_string(&body_a["tools"]).unwrap(),
        serde_json::to_string(&body_b["tools"]).unwrap(),
        "tool prefix must be unaffected by a dynamic-tail change"
    );
    // And, crucially, the stable-prefix HASH must be identical. The trailing
    // message breakpoint moved to the new last message (different content), but
    // that segment is deliberately excluded from the hash, so the guard must NOT
    // see drift here.
    assert_eq!(
        AnthropicProvider::stable_prefix_hash(&body_a),
        AnthropicProvider::stable_prefix_hash(&body_b),
        "a dynamic-tail (trailing message) change must not move the stable-prefix hash"
    );
}

/// Regression for the B3 drift-guard bug: two requests that share an identical
/// system + tool prefix but differ in their conversation messages — exactly what
/// consecutive turns of a growing conversation look like — must hash to the SAME
/// stable prefix. Before the fix the hash folded the trailing message breakpoint,
/// so this changed every turn and the guard warned on every single request.
#[test]
fn test_stable_prefix_hash_stable_across_growing_conversation() {
    let provider = test_provider();
    let (conv, tools) = drift_guard_fixture();

    // Turn 1: the fixture conversation as-is.
    let turn_1 = provider.build_request(&conv, &tools, None);

    // Turn 2: the conversation has grown — append an assistant reply and a new
    // user message, mirroring a real multi-turn session. The trailing breakpoint
    // now sits on a different message, but system + tools are untouched.
    let mut grown = conv.clone();
    grown.push(crate::message::Message::assistant("an assistant reply"));
    grown.push(crate::message::Message::user(
        "a follow-up that grows the convo",
    ));
    let turn_2 = provider.build_request(&grown, &tools, None);

    // Sanity: the message arrays genuinely differ (otherwise the test is vacuous).
    assert_ne!(
        turn_1["messages"], turn_2["messages"],
        "the two turns must have different messages for this regression to be meaningful"
    );

    let hash_1 = AnthropicProvider::stable_prefix_hash(&turn_1)
        .expect("stable tool/system markers present => hash should exist");
    let hash_2 = AnthropicProvider::stable_prefix_hash(&turn_2)
        .expect("stable tool/system markers present => hash should exist");
    assert_eq!(
        hash_1, hash_2,
        "identical system+tools across growing conversation turns must hash identically"
    );
}

/// Companion to the regression above: a mutated system block OR a mutated tool
/// definition must still move the hash, so the guard retains its teeth for the
/// drift it is actually meant to catch.
#[test]
fn test_stable_prefix_hash_detects_system_or_tool_mutation() {
    let provider = test_provider();
    let (conv, tools) = drift_guard_fixture();
    let baseline = provider.build_request(&conv, &tools, None);
    let baseline_hash = AnthropicProvider::stable_prefix_hash(&baseline).unwrap();

    // Mutated system block (the cached project-context text drifts).
    let mut conv_sys = conv.clone();
    conv_sys.messages[0].content[1] = ContentBlock::Text {
        text: "project context / repo map (DRIFTED)".to_string(),
    };
    let sys_body = provider.build_request(&conv_sys, &tools, None);
    assert_ne!(
        baseline_hash,
        AnthropicProvider::stable_prefix_hash(&sys_body).unwrap(),
        "a mutated cached system block must change the stable-prefix hash"
    );

    // Mutated tool definition (the cached tool schema/description drifts).
    let mut tools_mut = tools.clone();
    tools_mut[0]["description"] = json!("Run shell (description drifted)");
    let tool_body = provider.build_request(&conv, &tools_mut, None);
    assert_ne!(
        baseline_hash,
        AnthropicProvider::stable_prefix_hash(&tool_body).unwrap(),
        "a mutated cached tool definition must change the stable-prefix hash"
    );
}

/// The hash folds objects in sorted-key order, so it is independent of physical
/// map storage order even if `serde_json`'s `preserve_order` feature is ever
/// enabled. Build the same object with keys inserted in two different orders and
/// assert the fold produces the same hash.
#[test]
fn test_stable_prefix_hash_is_key_order_independent() {
    // Two tool arrays with the same logical content but different key insertion
    // order. With default serde_json (BTreeMap) these already serialize the
    // same, but the explicit sorted-key fold makes the guarantee robust.
    let body_a = json!({
        "model": "claude-3-5-sonnet",
        "tools": [{
            "cache_control": {"type": "ephemeral", "kind": "stable_prefix"},
            "name": "shell",
            "description": "Run shell"
        }]
    });
    let body_b = json!({
        "model": "claude-3-5-sonnet",
        "tools": [{
            "description": "Run shell",
            "name": "shell",
            "cache_control": {"kind": "stable_prefix", "type": "ephemeral"}
        }]
    });
    assert_eq!(
        AnthropicProvider::stable_prefix_hash(&body_a),
        AnthropicProvider::stable_prefix_hash(&body_b),
        "stable-prefix hash must be independent of object key order"
    );
}

/// No cache markers => no prefix to guard => `None` (guard stays silent).
#[test]
fn test_stable_prefix_hash_none_when_no_markers() {
    let body = json!({
        "model": "claude-3-5-sonnet",
        "system": "plain string system, no cache_control",
        "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]
    });
    assert!(
        AnthropicProvider::stable_prefix_hash(&body).is_none(),
        "a request with no cache_control markers has no stable prefix to hash"
    );
}

// ─── Default caching policy (metadata-less agentic conversations) ─────────

/// Worker/task sessions assemble their system prompt as one plain string
/// with no breakpoint metadata. With tools present, the default policy must
/// still cache: marker on the last tool, on the (single) system block, and
/// the trailing message breakpoint — 3 markers, within the cap of 4.
#[test]
fn test_default_cache_policy_marks_tools_system_and_trailing_message() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::system("worker system prompt"));
    conv.push(crate::message::Message::user("do the task"));

    let tools = vec![
        json!({"name": "shell", "description": "Run shell", "input_schema": {"type": "object"}}),
        json!({"name": "read", "description": "Read file", "input_schema": {"type": "object"}}),
    ];

    let req = provider.build_request(&conv, &tools, None);

    // Marker on the LAST tool only (breakpoint = end of cacheable prefix).
    assert!(req["tools"][0].get("cache_control").is_none());
    assert_eq!(
        req["tools"][1]["cache_control"],
        json!({"type": "ephemeral"})
    );

    // The single system block is marked, forcing array serialization.
    let system = req["system"].as_array().expect("system array");
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["text"], "worker system prompt");
    assert_eq!(system[0]["cache_control"], json!({"type": "ephemeral"}));

    // Trailing message breakpoint present.
    let messages = req["messages"].as_array().expect("messages");
    let last_block = messages.last().unwrap()["content"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last_block["cache_control"], json!({"type": "ephemeral"}));

    assert_eq!(count_cache_markers(&req), 3);
}

/// One-shot utility calls (no tools, no metadata — e.g. compaction
/// summaries) must stay unmarked: a cache write that is never read back is
/// pure cost.
#[test]
fn test_default_cache_policy_inactive_without_tools() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::system("summarise this"));
    conv.push(crate::message::Message::user("transcript…"));

    let req = provider.build_request(&conv, &[], None);
    assert_eq!(count_cache_markers(&req), 0);
    assert!(req["system"].is_string());
}

/// Explicit breakpoint metadata wins over the default policy: the system
/// split stays all-but-last (dynamic tail uncached).
#[test]
fn test_explicit_metadata_overrides_default_policy() {
    let provider = test_provider();
    let (conv, tools) = drift_guard_fixture();
    let req = provider.build_request(&conv, &tools, None);

    let system = req["system"].as_array().expect("system array");
    assert_eq!(system.len(), 3);
    assert!(system[0].get("cache_control").is_some());
    assert!(system[1].get("cache_control").is_some());
    assert!(
        system[2].get("cache_control").is_none(),
        "dynamic tail must stay uncached under the explicit contract"
    );
}

// ─── effective_url: Anthropic-compatible base URLs ────────────────────────

#[test]
fn test_effective_url_joins_native_and_v1_suffixed_bases() {
    let mut config = test_anthropic_config();
    config.base_url = "https://api.anthropic.com".to_string();
    assert_eq!(
        AnthropicProvider::new(config.clone()).effective_url(),
        "https://api.anthropic.com/v1/messages"
    );

    // MiniMax coding plan publishes a base that already ends in /v1.
    config.base_url = "https://api.minimax.io/anthropic/v1".to_string();
    assert_eq!(
        AnthropicProvider::new(config.clone()).effective_url(),
        "https://api.minimax.io/anthropic/v1/messages"
    );

    config.base_url = "https://api.minimax.io/anthropic/v1/".to_string();
    assert_eq!(
        AnthropicProvider::new(config).effective_url(),
        "https://api.minimax.io/anthropic/v1/messages"
    );
}

// ─── RMCP → Anthropic tool-shape conversion ───────────────────────────────

/// djinn's tool registry hands providers RMCP-shaped tools
/// (`{"name","description","inputSchema"}`). The Anthropic wire format
/// requires `input_schema`; the serializer must convert and emit a clean
/// object (no stray `inputSchema` key — strict Anthropic-compatible
/// vendors reject requests whose tools have no `input_schema`).
#[test]
fn test_rmcp_tools_converted_to_anthropic_input_schema() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));

    let tools = vec![json!({
        "name": "epic_list",
        "description": "List epics",
        "inputSchema": {"type": "object", "properties": {"project": {"type": "string"}}}
    })];

    let req = provider.build_request(&conv, &tools, None);
    let tool = &req["tools"][0];
    assert_eq!(tool["name"], "epic_list");
    assert_eq!(tool["description"], "List epics");
    assert_eq!(
        tool["input_schema"]["properties"]["project"]["type"],
        "string"
    );
    assert!(
        tool.get("inputSchema").is_none(),
        "camelCase RMCP key must not leak onto the wire"
    );
}

/// A tool with neither schema key still gets a minimal valid input_schema.
#[test]
fn test_tool_without_schema_gets_default_input_schema() {
    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));

    let tools = vec![json!({"name": "ping", "description": "Ping"})];
    let req = provider.build_request(&conv, &tools, None);
    assert_eq!(req["tools"][0]["input_schema"], json!({"type": "object"}));
}

// ─── 41g8 Wave 1: tool_schema_compat projection seam (Anthropic) ────────────

/// Native Anthropic / no-quirk config (`tool_schema_compat = None`) must
/// retain representative existing `input_schema` shape byte-for-byte: the
/// shared projection core is identity when `compat` is `None`, so a complex
/// RMCP-style schema (with `$ref`, `prefixItems`, `unevaluatedItems`, etc.)
/// passes through unchanged. This guards against a regression where an
/// `Some(...)` slipped through the seam and stripped native-Anthropic fields.
#[test]
fn test_tool_schema_compat_none_preserves_native_anthropic_input_schema() {
    use crate::provider::FormatFamily;
    use crate::provider::format::tool_projection;

    let provider = test_provider();
    let mut conv = Conversation::default();
    conv.push(crate::message::Message::user("hello"));

    // A schema that includes every keyword the shared Moonshot rewrite would
    // strip — so any accidental Moonshot application would be obvious.
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
    let tools = vec![json!({
        "name": "annotate",
        "description": "Annotate something",
        "input_schema": schema.clone()
    })];

    // Sanity: tool_projection identity path on a schema that contains those
    // keywords keeps them intact. This is the contract the Anthropic seam
    // must honour when `tool_schema_compat` is `None`.
    assert_eq!(
        tool_projection::project(schema.clone(), None, FormatFamily::Anthropic),
        schema,
        "shared projection must be identity when compat is None"
    );

    let req = provider.build_request(&conv, &tools, None);
    let serialized_schema = &req["tools"][0]["input_schema"];

    // Every Moonshot-stripped keyword survives on the wire for native Anthropic.
    assert_eq!(serialized_schema["$defs"]["point"]["type"], "object");
    assert!(
        serialized_schema["properties"]["point"]
            .get("$ref")
            .is_some()
    );
    assert!(
        serialized_schema["properties"]["point"]
            .get("prefixItems")
            .is_some()
    );
    assert!(
        serialized_schema["properties"]["point"]
            .get("unevaluatedItems")
            .is_some()
    );
}

/// Moonshot-compatible config (`tool_schema_compat = Moonshot`) on an
/// Anthropic-format provider (Kimi / Moonshot / MiniMax coding plan) must run
/// the converted `input_schema` through the shared projection core: `$ref`
/// sibling keywords are stripped, `prefixItems` collapses into `items`, and
/// `unevaluatedItems` is removed. Cache-control marker behavior on the tool
/// array remains intact — the projection is applied before cache_control is
/// inserted on the last tool, so marker placement is unaffected.
#[test]
fn test_tool_schema_compat_moonshot_projects_anthropic_input_schema() {
    use crate::provider::ToolSchemaCompat;

    let mut config = test_anthropic_config();
    config.tool_schema_compat = Some(ToolSchemaCompat::Moonshot);
    let provider = AnthropicProvider::new(config);

    let mut conv = Conversation::default();
    conv.push(crate::message::Message::system_with_metadata(
        "base prompt",
        crate::message::MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(json!({
                ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                    kind: Some("stable_prefix".to_string()),
                }
            })),
        },
    ));
    conv.messages[0].content.push(ContentBlock::Text {
        text: "project context".to_string(),
    });
    conv.messages[0].content.push(ContentBlock::Text {
        text: "dynamic tail".to_string(),
    });
    conv.push(crate::message::Message::user("hello"));

    // RMCP-shaped tool: schema arrives via `inputSchema`, includes every
    // Moonshot-rewritten keyword, and is paired with a plain tool that needs
    // no rewriting — to confirm both go through the projection step.
    //
    // Two distinct sub-trees exercise the rewrites:
    //   * a `$ref` with non-informational sibling keywords → siblings stripped.
    //   * an array schema with `prefixItems` + `unevaluatedItems` →
    //     prefixItems collapsed into items, unevaluatedItems dropped.
    let tools = vec![
        json!({
            "name": "complex",
            "description": "Tool with full Moonshot rewrite coverage",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ref_field": {
                        "$ref": "#/$defs/complex",
                        "minLength": 0,
                        "pattern": ".*",
                        "description": "informational sibling kept by Moonshot"
                    },
                    "coord": {
                        "type": "array",
                        "prefixItems": [{"type": "number"}, {"type": "number"}],
                        "unevaluatedItems": false
                    }
                },
                "$defs": {
                    "complex": {"type": "object"}
                }
            }
        }),
        json!({
            "name": "plain",
            "description": "Tool with no Moonshot rewrite needed",
            "input_schema": {"type": "object", "properties": {"x": {"type": "string"}}}
        }),
    ];

    let req = provider.build_request(&conv, &tools, None);
    let req_tools = req["tools"].as_array().expect("tools array");
    assert_eq!(req_tools.len(), 2);

    // First tool: RMCP `inputSchema` was converted to Anthropic `input_schema`
    // AND the schema body was projected through the Moonshot rewrites.
    let complex = &req_tools[0];
    assert_eq!(complex["name"], "complex");
    assert!(
        complex.get("inputSchema").is_none(),
        "RMCP camelCase key must not leak onto the wire"
    );
    let projected = &complex["input_schema"];
    let ref_field = &projected["properties"]["ref_field"];
    let coord = &projected["properties"]["coord"];

    // $ref kept; non-informational sibling keywords stripped by Moonshot.
    assert!(ref_field.get("$ref").is_some());
    assert!(
        ref_field.get("minLength").is_none(),
        "Moonshot projection must strip non-informational $ref sibling keywords"
    );
    assert!(
        ref_field.get("pattern").is_none(),
        "Moonshot projection must strip non-informational $ref sibling keywords"
    );
    // Description is on the Moonshot keep-list, so it survives.
    assert_eq!(
        ref_field["description"],
        "informational sibling kept by Moonshot"
    );

    // prefixItems collapsed into items, unevaluatedItems dropped.
    assert!(
        coord.get("prefixItems").is_none(),
        "Moonshot projection must collapse prefixItems into items"
    );
    assert!(
        coord.get("unevaluatedItems").is_none(),
        "Moonshot projection must drop unevaluatedItems"
    );
    assert!(
        coord["items"].is_array(),
        "collapsed items array must be present"
    );
    let items = coord["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["type"], "number");
    assert_eq!(items[1]["type"], "number");

    // Second tool: identity for keywords Moonshot does not touch.
    let plain = &req_tools[1];
    assert_eq!(plain["input_schema"]["type"], "object");
    assert_eq!(plain["input_schema"]["properties"]["x"]["type"], "string");

    // Cache-control marker behavior remains intact: the last tool definition
    // (and only the last) carries the `ephemeral` marker, system prefix blocks
    // still receive cache_control, and the trailing message breakpoint is
    // still emitted. Projection must not perturb any of this.
    assert!(
        req_tools[0].get("cache_control").is_none(),
        "the non-last tool must not carry a cache_control marker"
    );
    assert_eq!(
        req_tools[1]["cache_control"],
        json!({"type": "ephemeral"}),
        "the last tool definition still owns the cacheable-prefix marker"
    );

    let system = req["system"].as_array().expect("system array");
    assert_eq!(system[0]["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(system[1]["cache_control"], json!({"type": "ephemeral"}));
    assert!(system[2].get("cache_control").is_none());

    let messages = req["messages"].as_array().expect("messages");
    let last_block = messages.last().unwrap()["content"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last_block["cache_control"], json!({"type": "ephemeral"}));
}

/// Default `tool_schema_compat = None` paths still produce the existing
/// `cache_control` marker placement after the seam change — a regression
/// guard so adding `compat` to the serializer cannot silently move the
/// tool-`cache_control` marker off the last definition.
#[test]
fn test_tool_schema_compat_none_cache_control_marker_unchanged() {
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
                    kind: Some("stable_prefix".to_string()),
                }
            })),
        },
    ));
    conv.messages[0].content.push(ContentBlock::Text {
        text: "project context".to_string(),
    });
    conv.push(crate::message::Message::user("hello"));

    let tools = vec![
        json!({"name": "first", "description": "first", "input_schema": {"type": "object"}}),
        json!({"name": "second", "description": "second", "input_schema": {"type": "object"}}),
    ];

    let req = provider.build_request(&conv, &tools, None);
    let req_tools = req["tools"].as_array().expect("tools array");
    assert!(req_tools[0].get("cache_control").is_none());
    assert_eq!(req_tools[1]["cache_control"], json!({"type": "ephemeral"}));
}
