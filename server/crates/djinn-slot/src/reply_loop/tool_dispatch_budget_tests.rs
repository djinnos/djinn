// The telemetry guard mutex is intentionally held across awaits to serialize tests.
#![allow(clippy::await_holding_lock)]
// Turn-inline-budget and `collect_tool_results` coverage split out of
// `tool_dispatch_tests` to keep each test module under the source-size guard.
// Shared helpers (`test_tool_schema`, `test_cancel_token`) remain in the sibling
// `tool_dispatch_tests` module and are imported here.
use super::super::turn_budget::{
    DEFAULT_TURN_INLINE_CHAR_BUDGET, DEFAULT_TURN_INLINE_PREVIEW_FLOOR, TurnInlineBudgetConfig,
    apply_turn_inline_budget_pass_with_config, read_positive_env_usize,
};
use super::tool_dispatch_tests::{
    test_cancel_token, test_tool_schema, turn_budget_telemetry_guard,
};
use super::*;
use djinn_telemetry::render;

fn test_dispatch_context<'a>(
    ctx: &'a SlotContext,
    tool_metadata: &'a ToolRuntimeMetadataMap,
    worktree_path: &'a std::path::Path,
) -> ToolDispatchContext<'a> {
    ToolDispatchContext {
        ctx,
        task_id: "test-task",
        worktree_path,
        role_name: "test-role",
        tool_metadata,
        tool_dispatcher: ctx.tool_dispatcher.as_ref().unwrap().as_ref(),
        otel_session: None,
        phase_tracker: None,
        cancel: test_cancel_token(),
    }
}
#[tokio::test]
async fn collect_tool_results_preserves_names_and_ordering_and_applies_turn_budget_pass() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use std::collections::HashSet;
    use tokio_util::sync::CancellationToken;
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let schemas = vec![
        test_tool_schema(
            "shell",
            Some(true),
            Some(false),
            Some(true),
            Some(false),
            Some(false),
        ),
        test_tool_schema(
            "read",
            Some(true),
            Some(false),
            Some(true),
            Some(false),
            Some(true),
        ),
        test_tool_schema(
            "code_search",
            Some(true),
            Some(false),
            Some(true),
            Some(false),
            Some(true),
        ),
    ];
    let tool_metadata = tool_runtime_metadata(&schemas);
    let turn_tool_calls = vec![
        ContentBlock::ToolUse {
            id: "call-0".into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        },
        ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "read".into(),
            input: serde_json::json!({}),
        },
        ContentBlock::ToolUse {
            id: "call-2".into(),
            name: "code_search".into(),
            input: serde_json::json!({}),
        },
        ContentBlock::ToolUse {
            id: "call-3".into(),
            name: "write".into(),
            input: serde_json::json!({}),
        },
    ];
    let streaming_results = vec![(
        3,
        ContentBlock::ToolResult {
            tool_use_id: "call-3".into(),
            content: vec![ContentBlock::text("streamed write ok")],
            is_error: false,
        },
    )];
    let streaming_dispatched = HashSet::from([3]);
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let collected = collect_tool_results_internal(
        &turn_tool_calls,
        streaming_results,
        &streaming_dispatched,
        &tool_metadata,
        &dispatch_ctx,
    )
    .await;
    assert_eq!(collected.len(), 4);
    assert_eq!(collected[0].idx, 0);
    assert_eq!(collected[1].idx, 1);
    assert_eq!(collected[2].idx, 2);
    assert_eq!(collected[3].idx, 3);
    assert_eq!(collected[0].tool_name, "shell");
    assert_eq!(collected[1].tool_name, "read");
    assert_eq!(collected[2].tool_name, "code_search");
    assert_eq!(collected[3].tool_name, "write");
    assert!(!collected.iter().any(|r| r.name_missing));
    let blocks: Vec<ContentBlock> = collected
        .into_iter()
        .map(CollectedToolResult::into_content_block)
        .collect();
    let ids: Vec<String> = blocks
        .iter()
        .map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id.clone(),
            _ => panic!("expected ToolResult"),
        })
        .collect();
    assert_eq!(ids, vec!["call-0", "call-1", "call-2", "call-3"]);
    let rendered_results: Vec<(String, String, bool)> = blocks
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let [ContentBlock::Text { text }] = content.as_slice() else {
                    panic!("expected text result");
                };
                (tool_use_id.clone(), text.clone(), *is_error)
            }
            _ => panic!("expected text result"),
        })
        .collect();
    assert_eq!(
        rendered_results,
        vec![
            (
                "call-0".to_string(),
                "{\n  \"ok\": true,\n  \"exit_code\": 0,\n  \"stdout\": \"mock shell output\\n\",\n  \"stderr\": \"\",\n  \"workdir\": \"/tmp\"\n}"
                    .to_string(),
                false,
            ),
            ("call-1".to_string(), "{\n  \"ok\": true\n}".to_string(), false),
            ("call-2".to_string(), "{\n  \"ok\": true\n}".to_string(), false),
            ("call-3".to_string(), "streamed write ok".to_string(), false),
        ]
    );
}
#[tokio::test]
async fn collect_tool_results_budget_pass_externalizes_largest_serial_parallel_streaming() {
    use crate::test_helpers::{
        ConfigurableToolDispatcher, ToolHandlerFn, agent_context_from_db_with_dispatcher,
        create_test_db,
    };
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    let db = create_test_db();
    let mut handlers: HashMap<String, ToolHandlerFn> = HashMap::new();
    handlers.insert(
        "shell".to_string(),
        (|_| {
            Ok(serde_json::json!({
                "ok": true,
                "exit_code": 0,
                "stdout": "A".repeat(40_000),
                "stderr": "",
                "workdir": "/tmp"
            }))
        }) as ToolHandlerFn,
    );
    handlers.insert(
        "read".to_string(),
        (|_| Ok(serde_json::json!({"output": "B".repeat(40_000)}))) as ToolHandlerFn,
    );
    handlers.insert(
        "code_search".to_string(),
        (|_| Ok(serde_json::json!({"output": "C".repeat(40_000)}))) as ToolHandlerFn,
    );
    handlers.insert(
        "write".to_string(),
        (|_| Ok(serde_json::json!({"ok": true}))) as ToolHandlerFn,
    );
    let dispatcher = Arc::new(ConfigurableToolDispatcher::new(Vec::new(), handlers));
    let ctx = agent_context_from_db_with_dispatcher(db, CancellationToken::new(), Some(dispatcher));
    let worktree_path = std::path::Path::new("/tmp");
    unsafe {
        std::env::set_var("DJINN_TURN_INLINE_CHAR_BUDGET", "5000");
        std::env::set_var("DJINN_TURN_INLINE_PREVIEW_FLOOR", "500");
    }
    let schemas = vec![
        test_tool_schema(
            "shell",
            Some(true),
            Some(false),
            Some(true),
            Some(false),
            Some(false),
        ),
        test_tool_schema(
            "read",
            Some(true),
            Some(false),
            Some(true),
            Some(false),
            Some(true),
        ),
        test_tool_schema(
            "code_search",
            Some(true),
            Some(false),
            Some(true),
            Some(false),
            Some(true),
        ),
    ];
    let tool_metadata = tool_runtime_metadata(&schemas);
    let turn_tool_calls = vec![
        ContentBlock::ToolUse {
            id: "call-0".into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        },
        ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "read".into(),
            input: serde_json::json!({}),
        },
        ContentBlock::ToolUse {
            id: "call-2".into(),
            name: "code_search".into(),
            input: serde_json::json!({}),
        },
        ContentBlock::ToolUse {
            id: "call-3".into(),
            name: "write".into(),
            input: serde_json::json!({}),
        },
    ];
    let streaming_results = vec![(
        3,
        ContentBlock::ToolResult {
            tool_use_id: "call-3".into(),
            content: vec![ContentBlock::text("D".repeat(40_000))],
            is_error: false,
        },
    )];
    let streaming_dispatched = HashSet::from([3]);
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let blocks = collect_tool_results(
        &turn_tool_calls,
        streaming_results,
        &streaming_dispatched,
        &tool_metadata,
        &dispatch_ctx,
    )
    .await;
    let ids: Vec<String> = blocks
        .iter()
        .map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id.clone(),
            _ => panic!("expected ToolResult"),
        })
        .collect();
    assert_eq!(ids, vec!["call-0", "call-1", "call-2", "call-3"]);
    for (idx, expected_id, expected_name) in [
        (0, "call-0", "shell"),
        (1, "call-1", "read"),
        (2, "call-2", "code_search"),
        (3, "call-3", "write"),
    ] {
        let text = match &blocks[idx] {
            ContentBlock::ToolResult { content, .. } => match &content[0] {
                ContentBlock::Text { text } => text.clone(),
                _ => panic!("expected text"),
            },
            _ => panic!("expected ToolResult"),
        };
        assert!(
            text.starts_with("[djinn-output-stash"),
            "externalization failed"
        );
        assert!(text.contains(&format!("tool_use_id=\"{expected_id}\"")));
        assert!(text.contains(&format!("tool_name=\"{expected_name}\"")));
        assert!(text.contains("reason=\"turn_budget\""));
    }
}
#[tokio::test]
async fn collect_tool_results_uses_unknown_tool_for_nameless_input() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use std::collections::HashSet;
    use tokio_util::sync::CancellationToken;
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let turn_tool_calls = vec![ContentBlock::ToolUse {
        id: "call-0".into(),
        name: "shell".into(),
        input: serde_json::json!({}),
    }];
    let streaming_results = vec![(
        5,
        ContentBlock::ToolResult {
            tool_use_id: "call-5".into(),
            content: vec![ContentBlock::text("orphan result")],
            is_error: true,
        },
    )];
    let streaming_dispatched = HashSet::from([5]);
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let collected = collect_tool_results_internal(
        &turn_tool_calls,
        streaming_results,
        &streaming_dispatched,
        &tool_metadata,
        &dispatch_ctx,
    )
    .await;
    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0].idx, 0);
    assert_eq!(collected[0].tool_name, "shell");
    assert!(!collected[0].name_missing);
    assert_eq!(collected[1].idx, 5);
    assert_eq!(collected[1].tool_name, UNKNOWN_TOOL_NAME);
    assert!(collected[1].name_missing);
}
#[tokio::test]
async fn collect_tool_results_preserves_mcp_and_extension_names() {
    use crate::test_helpers::{
        ConfigurableToolDispatcher, ToolHandlerFn, agent_context_from_db_with_dispatcher,
        create_test_db,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    let db = create_test_db();
    let mut handlers: HashMap<String, ToolHandlerFn> = HashMap::new();
    handlers.insert(
        "mcp_fetch".to_string(),
        (|_| Ok(serde_json::json!({"ok": true}))) as ToolHandlerFn,
    );
    handlers.insert(
        "extension_compute".to_string(),
        (|_| Ok(serde_json::json!({"result": 42}))) as ToolHandlerFn,
    );
    let dispatcher = Arc::new(ConfigurableToolDispatcher::new(
        vec!["mcp_fetch".to_string()],
        handlers,
    ));
    let ctx = agent_context_from_db_with_dispatcher(db, CancellationToken::new(), Some(dispatcher));
    let worktree_path = std::path::Path::new("/tmp");
    let schemas = vec![
        test_tool_schema(
            "mcp_fetch",
            Some(true),
            Some(false),
            Some(true),
            Some(false),
            Some(true),
        ),
        test_tool_schema(
            "extension_compute",
            Some(true),
            Some(false),
            Some(true),
            Some(false),
            Some(true),
        ),
    ];
    let tool_metadata = tool_runtime_metadata(&schemas);
    let turn_tool_calls = vec![
        ContentBlock::ToolUse {
            id: "mcp-1".into(),
            name: "mcp_fetch".into(),
            input: serde_json::json!({}),
        },
        ContentBlock::ToolUse {
            id: "ext-1".into(),
            name: "extension_compute".into(),
            input: serde_json::json!({}),
        },
    ];
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let collected = collect_tool_results_internal(
        &turn_tool_calls,
        Vec::new(),
        &HashSet::new(),
        &tool_metadata,
        &dispatch_ctx,
    )
    .await;
    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0].tool_name, "mcp_fetch");
    assert_eq!(collected[1].tool_name, "extension_compute");
    assert!(!collected.iter().any(|r| r.name_missing));
}
#[tokio::test]
async fn collect_tool_results_preserves_stash_tool_name() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let turn_tool_calls = vec![ContentBlock::ToolUse {
        id: "stash-1".into(),
        name: "output_view".into(),
        input: serde_json::json!({"tool_use_id": "prior"}),
    }];
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let collected = collect_tool_results_internal(
        &turn_tool_calls,
        Vec::new(),
        &HashSet::new(),
        &tool_metadata,
        &dispatch_ctx,
    )
    .await;
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].tool_name, "output_view");
    assert!(!collected[0].name_missing);
}
fn collected_text(
    idx: usize,
    tool_use_id: &str,
    tool_name: &str,
    text: &str,
) -> CollectedToolResult {
    CollectedToolResult {
        idx,
        tool_use_id: tool_use_id.to_string(),
        tool_name: tool_name.to_string(),
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        is_error: false,
        name_missing: false,
    }
}
#[test]
fn config_defaults_match_specification() {
    assert_eq!(DEFAULT_TURN_INLINE_CHAR_BUDGET, 100_000);
    assert_eq!(DEFAULT_TURN_INLINE_PREVIEW_FLOOR, 10_000);
}
#[test]
fn config_reads_validated_env_overrides() {
    assert_eq!(
        read_positive_env_usize("DJINN_TEST_BUDGET_OVERRIDE_NONEXISTENT", 42),
        42,
        "unset var falls back to default"
    );
    let config = TurnInlineBudgetConfig {
        budget: 100_000,
        preview_floor: 10_000,
    };
    assert_eq!(config.budget, DEFAULT_TURN_INLINE_CHAR_BUDGET);
    assert_eq!(config.preview_floor, DEFAULT_TURN_INLINE_PREVIEW_FLOOR);
}
#[tokio::test]
async fn under_budget_turn_is_unchanged_byte_for_byte() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let body = "x".repeat(1_000);
    let mut results = vec![collected_text(0, "call-0", "read", &body)];
    let snapshot_before: Vec<String> = results
        .iter()
        .map(|r| match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        })
        .collect();
    let config = TurnInlineBudgetConfig {
        budget: 100_000_000,
        preview_floor: 10_000,
    };
    apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);
    let snapshot_after: Vec<String> = results
        .iter()
        .map(|r| match &r.content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        })
        .collect();
    assert_eq!(snapshot_before, snapshot_after, "result changed");
}
#[tokio::test]
async fn largest_first_selection_externalizes_the_biggest_candidate() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let config = TurnInlineBudgetConfig {
        budget: 200,
        preview_floor: 10,
    };
    let big = "B".repeat(5_000);
    let small = "S".repeat(500);
    let mut results = vec![
        collected_text(0, "call-big", "shell", &big),
        collected_text(1, "call-small", "read", &small),
    ];
    apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);
    let big_text = match &results[0].content[0] {
        ContentBlock::Text { text } => text.as_str(),
        _ => panic!("expected text"),
    };
    assert!(
        big_text.starts_with("[djinn-output-stash"),
        "largest not externalized: {}",
        &big_text[..big_text.len().min(80)]
    );
    assert!(big_text.contains("reason=\"turn_budget\""));
    assert!(big_text.contains("tool_name=\"shell\""));
}
#[tokio::test]
async fn non_shrinking_stub_is_skipped() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let config = TurnInlineBudgetConfig {
        budget: 50,
        preview_floor: 40,
    };
    let body = "x".repeat(41);
    let original = body.clone();
    let mut results = vec![collected_text(0, "call-0", "read", &body)];
    apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);
    let text = match &results[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("expected text"),
    };
    assert_eq!(text, original, "stub changed result");
}
#[tokio::test]
async fn preview_floor_prevents_fitting_allows_overflow() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let config = TurnInlineBudgetConfig {
        budget: 100,
        preview_floor: 10_000,
    };
    let body_a = "A".repeat(500);
    let body_b = "B".repeat(500);
    let original_a = body_a.clone();
    let original_b = body_b.clone();
    let mut results = vec![
        collected_text(0, "call-0", "read", &body_a),
        collected_text(1, "call-1", "read", &body_b),
    ];
    apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);
    let text_a = match &results[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("expected text"),
    };
    let text_b = match &results[1].content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("expected text"),
    };
    assert_eq!(text_a, original_a, "floor changed result");
    assert_eq!(text_b, original_b, "floor changed result");
}
#[tokio::test]
async fn externalization_preserves_extension_mcp_and_native_resource_recovery_metadata() {
    use crate::test_helpers::{
        ConfigurableToolDispatcher, ToolHandlerFn, agent_context_from_db_with_dispatcher,
        create_test_db,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    fn large_extension_output(
        _: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({"output": "E".repeat(5_000)}))
    }
    fn large_mcp_output(
        _: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({"output": "M".repeat(5_000)}))
    }
    let db = create_test_db();
    let mut handlers: HashMap<String, ToolHandlerFn> = HashMap::new();
    handlers.insert("mcp_fetch".to_string(), large_mcp_output as ToolHandlerFn);
    handlers.insert(
        "extension_compute".to_string(),
        large_extension_output as ToolHandlerFn,
    );
    let dispatcher = Arc::new(
        ConfigurableToolDispatcher::new(vec!["mcp_fetch".to_string()], handlers)
            .with_resource_results(HashMap::from([(
                "read_mcp_resource".to_string(),
                "R".repeat(5_000),
            )])),
    );
    let ctx = agent_context_from_db_with_dispatcher(db, CancellationToken::new(), Some(dispatcher));
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    unsafe {
        std::env::set_var("DJINN_TURN_INLINE_CHAR_BUDGET", "200");
        std::env::set_var("DJINN_TURN_INLINE_PREVIEW_FLOOR", "10");
    }
    let turn_tool_calls = vec![
        ContentBlock::ToolUse {
            id: "call-ext".into(),
            name: "extension_compute".into(),
            input: serde_json::json!({}),
        },
        ContentBlock::ToolUse {
            id: "call-mcp".into(),
            name: "mcp_fetch".into(),
            input: serde_json::json!({}),
        },
        ContentBlock::ToolUse {
            id: "call-res".into(),
            name: "read_mcp_resource".into(),
            input: serde_json::json!({}),
        },
    ];
    let results = collect_tool_results(
        &turn_tool_calls,
        Vec::new(),
        &std::collections::HashSet::new(),
        &tool_metadata,
        &dispatch_ctx,
    )
    .await;
    unsafe {
        std::env::remove_var("DJINN_TURN_INLINE_CHAR_BUDGET");
        std::env::remove_var("DJINN_TURN_INLINE_PREVIEW_FLOOR");
    }
    for (expected_id, expected_name) in [
        ("call-ext", "extension_compute"),
        ("call-mcp", "mcp_fetch"),
        ("call-res", "read_mcp_resource"),
    ] {
        let result = results
            .iter()
            .find(|block| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == expected_id))
            .unwrap_or_else(|| panic!("missing result for {expected_id}"));
        let text = match result {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(!is_error, "dispatch failed");
                match &content[0] {
                    ContentBlock::Text { text } => text.clone(),
                    _ => panic!("expected text"),
                }
            }
            _ => panic!("expected tool result"),
        };
        assert!(
            text.starts_with("[djinn-output-stash"),
            "externalization failed"
        );
        assert!(text.contains(&format!("tool_use_id=\"{expected_id}\"")));
        assert!(text.contains(&format!("tool_name=\"{expected_name}\"")));
        assert!(text.contains("reason=\"turn_budget\""));
    }
}
#[tokio::test]
async fn externalization_preserves_tool_use_id_and_name_in_stub() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let config = TurnInlineBudgetConfig {
        budget: 200,
        preview_floor: 10,
    };
    let big = "Z".repeat(5_000);
    let mut results = vec![collected_text(7, "call-preserve-id", "code_search", &big)];
    apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);
    let text = match &results[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        _ => panic!("expected text"),
    };
    assert!(text.contains("tool_use_id=\"call-preserve-id\""));
    assert!(text.contains("tool_name=\"code_search\""));
    assert!(text.contains("reason=\"turn_budget\""));
}
#[derive(Clone, Default)]
struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl CapturedLogs {
    fn output(&self) -> String {
        let buf = self.0.lock().expect("captured logs mutex poisoned");
        String::from_utf8(buf.clone()).expect("invalid log bytes")
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLogs {
    type Writer = CapturedLogsWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CapturedLogsWriter {
            inner: std::sync::Arc::clone(&self.0),
        }
    }
}
struct CapturedLogsWriter {
    inner: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}
impl std::io::Write for CapturedLogsWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner
            .lock()
            .expect("captured logs mutex poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn budget_trip_reports_tool_name_missing_for_unselected_nameless_result() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let config = TurnInlineBudgetConfig {
        budget: 200,
        preview_floor: 10,
    };
    let big = "B".repeat(5_000);
    let big_result = collected_text(0, "call-big", "shell", &big);
    let small_nameless = CollectedToolResult {
        idx: 5,
        tool_use_id: "call-5".to_string(),
        tool_name: UNKNOWN_TOOL_NAME.to_string(),
        content: vec![ContentBlock::Text {
            text: "orphan result".to_string(),
        }],
        is_error: true,
        name_missing: true,
    };
    let mut results = vec![big_result, small_nameless];
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(logs.clone())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
        .with_target(true)
        .with_ansi(false)
        .with_level(true)
        .finish();
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    let _guard = tracing::dispatcher::set_default(&dispatch);
    apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);
    let output = logs.output();
    assert!(
        output.contains("tool_name_missing=true"),
        "telemetry must report tool_name_missing=true when a nameless result \
         exists in the batch, even if it was not selected for externalization. \
         Got: {output}"
    );
}
fn budget_trip_counter_value(rendered: &str) -> f64 {
    rendered
        .lines()
        .find_map(|line| {
            line.strip_prefix("djinn_reply_loop_inline_char_budget_trips_total")
                .and_then(|suffix| suffix.strip_prefix(' '))
                .and_then(|value| value.parse::<f64>().ok())
        })
        .unwrap_or_else(|| {
            panic!(
                "missing unlabelled sample djinn_reply_loop_inline_char_budget_trips_total in:\n{rendered}"
            )
        })
}
#[tokio::test]
async fn under_budget_turn_does_not_increment_budget_trip_counter() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;
    let _guard = turn_budget_telemetry_guard();
    djinn_telemetry::init().expect("telemetry init");
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let before = render().expect("render metrics");
    let before_value = budget_trip_counter_value(&before);
    let config = TurnInlineBudgetConfig {
        budget: 100_000_000,
        preview_floor: 10_000,
    };
    let body = "x".repeat(1_000);
    let mut results = vec![collected_text(0, "call-0", "read", &body)];
    apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);
    let after = render().expect("render after pass");
    let after_value = budget_trip_counter_value(&after);
    assert_eq!(
        after_value, before_value,
        "under-budget turn must not increment the budget-trip counter:\n{after}"
    );
}
#[tokio::test]
async fn over_budget_turn_increments_budget_trip_counter_by_one_for_multiple_externalizations() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;
    let _guard = turn_budget_telemetry_guard();
    djinn_telemetry::init().expect("telemetry init");
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let before = render().expect("render metrics");
    let before_value = budget_trip_counter_value(&before);
    let config = TurnInlineBudgetConfig {
        budget: 200,
        preview_floor: 10,
    };
    let big_a = "A".repeat(5_000);
    let big_b = "B".repeat(5_000);
    let mut results = vec![
        collected_text(0, "call-a", "shell", &big_a),
        collected_text(1, "call-b", "read", &big_b),
    ];
    apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);
    let externalized = results
        .iter()
        .filter(|r| {
            matches!(
                r.content.first(),
                Some(ContentBlock::Text { text }) if text.starts_with("[djinn-output-stash")
            )
        })
        .count();
    assert!(externalized >= 2, "missing externalization");
    let after = render().expect("render after pass");
    let after_value = budget_trip_counter_value(&after);
    assert_eq!(after_value, before_value + 1.0, "bad counter increment");
}
#[tokio::test]
async fn over_budget_turn_increments_budget_trip_counter_by_one_when_residual_overflow_remains() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;
    let _guard = turn_budget_telemetry_guard();
    djinn_telemetry::init().expect("telemetry init");
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let before = render().expect("render metrics");
    let before_value = budget_trip_counter_value(&before);
    let config = TurnInlineBudgetConfig {
        budget: 100,
        preview_floor: 10_000,
    };
    let body_a = "A".repeat(500);
    let body_b = "B".repeat(500);
    let mut results = vec![
        collected_text(0, "call-0", "read", &body_a),
        collected_text(1, "call-1", "read", &body_b),
    ];
    apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);
    for result in &results {
        let text = match result.content.first() {
            Some(ContentBlock::Text { text }) => text.as_str(),
            _ => panic!("expected text block"),
        };
        assert!(
            !text.starts_with("[djinn-output-stash"),
            "floor externalized"
        );
    }
    let after = render().expect("render after pass with residual overflow");
    let after_value = budget_trip_counter_value(&after);
    assert_eq!(after_value, before_value + 1.0, "bad counter increment");
}
#[tokio::test]
async fn budget_trip_structured_event_retains_required_fields() {
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;
    let db = create_test_db();
    let ctx = agent_context_from_db(db, CancellationToken::new());
    let worktree_path = std::path::Path::new("/tmp");
    let tool_metadata = ToolRuntimeMetadataMap::new();
    let dispatch_ctx = test_dispatch_context(&ctx, &tool_metadata, worktree_path);
    let config = TurnInlineBudgetConfig {
        budget: 200,
        preview_floor: 10,
    };
    let big_a = "A".repeat(5_000);
    let big_b = "B".repeat(5_000);
    let mut results = vec![
        collected_text(0, "call-a", "shell", &big_a),
        collected_text(1, "call-b", "read", &big_b),
    ];
    let logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(logs.clone())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
        .with_target(true)
        .with_ansi(false)
        .with_level(true)
        .finish();
    let dispatch = tracing::dispatcher::Dispatch::new(subscriber);
    let _guard = tracing::dispatcher::set_default(&dispatch);
    apply_turn_inline_budget_pass_with_config(&mut results, &dispatch_ctx, config);
    let output = logs.output();
    for field in [
        "inline_chars_pre=",
        "inline_chars_post=",
        "tool_count=",
        "externalized_count=",
        "largest_result_chars=",
        "tool_name_missing=",
    ] {
        assert!(output.contains(field), "missing telemetry field");
    }
}
#[test]
fn budget_trip_counter_name_is_coupled_to_telemetry_constant() {
    let _guard = turn_budget_telemetry_guard();
    djinn_telemetry::init().expect("telemetry init");
    let before = render().expect("render metrics");
    let before_value = budget_trip_counter_value(&before);
    djinn_telemetry::reply_loop::increment_inline_char_budget_trip();
    let after = render().expect("render after increment");
    let after_value = budget_trip_counter_value(&after);
    assert_eq!(after_value, before_value + 1.0, "bad counter increment");
}
