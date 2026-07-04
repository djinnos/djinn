use super::config::ResolvedMcpServerConfig;
use super::tests::make_routing;
use super::*;
use std::collections::HashMap;
use std::time::Duration;

// ── rmcp capability access / adapter validation ────────────────────
//
// These compile-time probes verify that the rmcp API touchpoints
// needed by sibling epics (yjc6, hyeu) are accessible through the
// types already used in mcp_client.rs.  If a future rmcp upgrade
// removes or renames any of these fields, these tests will fail to
// compile — giving early warning before the sibling epics attempt
// to use them.
//
// NOT exposed to the model:
// - `InitializeResult.instructions` → yjc6 (prompt instructions block)
// - resource tools → hyeu (resources-as-tools)
//
// Validated access points:
// - `ServerCapabilities.tools` / `.resources` / `.logging` / `.prompts`
// - `ToolsCapability.list_changed`
// - `LoggingMessageNotification` type exists (notification handler shape)
// - `ServerNotification` enum variants for handler dispatch

#[test]
fn rmcp_initialize_result_instructions_field_is_accessible() {
    // Compile-time probe: InitializeResult has an `instructions` field.
    // Used by yjc6 to extract server prompt instructions.
    let result = rmcp::model::InitializeResult::new(rmcp::model::ServerCapabilities::default());
    // instructions defaults to None
    assert!(result.instructions.is_none());
    // Can set instructions
    let with_inst = result.with_instructions("test instructions");
    assert_eq!(with_inst.instructions.as_deref(), Some("test instructions"));
}

#[test]
fn rmcp_server_capabilities_fields_are_accessible() {
    // Compile-time probe: ServerCapabilities exposes tools, resources,
    // prompts, and logging fields.
    let caps = rmcp::model::ServerCapabilities::default();
    assert!(caps.tools.is_none());
    assert!(caps.resources.is_none());
    assert!(caps.prompts.is_none());
    assert!(caps.logging.is_none());
}

#[test]
fn rmcp_tools_capability_list_changed_is_accessible() {
    // Compile-time probe: ToolsCapability.list_changed indicates whether
    // the server supports tools/list_changed notifications.
    let tc = rmcp::model::ToolsCapability {
        list_changed: Some(true),
    };
    assert_eq!(tc.list_changed, Some(true));
}

#[test]
fn rmcp_logging_notification_type_is_accessible() {
    // Compile-time probe: LoggingMessageNotification exists and can be
    // pattern-matched from ServerNotification. This is the type that a
    // notification handler/channel adapter would need to receive.
    use rmcp::model::{LoggingLevel, LoggingMessageNotificationParam, ServerNotification};

    // Construct a minimal logging notification.
    let logging =
        LoggingMessageNotificationParam::new(LoggingLevel::Info, serde_json::json!("test message"));

    // Verify the param fields are accessible.
    assert_eq!(logging.level, LoggingLevel::Info);
    assert_eq!(logging.data, serde_json::json!("test message"));

    // Wrap in ServerNotification to confirm the variant exists and is matchable.
    let notif =
        ServerNotification::LoggingMessageNotification(rmcp::model::Notification::new(logging));

    // Pattern-match to verify the handler dispatch shape.
    match notif {
        ServerNotification::LoggingMessageNotification(inner) => {
            assert_eq!(inner.params.level, LoggingLevel::Info);
        }
        _ => panic!("expected LoggingMessageNotification variant"),
    }
}

#[test]
fn rmcp_tool_list_changed_notification_type_is_accessible() {
    // Compile-time probe: ToolListChangedNotification exists and can be
    // constructed/matched. This is the type that a notification handler
    // for tools/list_changed would receive.
    use rmcp::model::ServerNotification;

    // ToolListChangedNotification is NotificationNoParam<Method>, which
    // derives Default since the Method is a unit struct.
    let changed: rmcp::model::ToolListChangedNotification = Default::default();
    let notif = ServerNotification::ToolListChangedNotification(changed);

    match notif {
        ServerNotification::ToolListChangedNotification(_) => {
            // Successfully matched — the variant and type are accessible.
        }
        _ => panic!("expected ToolListChangedNotification variant"),
    }
}

#[test]
fn startup_timeout_defaults_match_config() {
    // Verify the startup/request timeout defaults from McpServerConfig
    // match the expected values.
    assert_eq!(McpServerConfig::default_startup_timeout_ms(), 30_000);
    assert_eq!(McpServerConfig::default_request_timeout_ms(), 120_000);
}

#[test]
fn resolved_config_startup_and_request_timeouts_from_duration_helpers() {
    let config = ResolvedMcpServerConfig {
        url: Some("https://example.com/mcp".to_string()),
        command: None,
        args: Vec::new(),
        env: HashMap::new(),
        headers: HashMap::new(),
        startup_timeout_ms: 5_000,
        request_timeout_ms: 30_000,
    };
    assert_eq!(config.startup_timeout(), Duration::from_millis(5_000));
    assert_eq!(config.request_timeout(), Duration::from_millis(30_000));
}

// ── Notification handler / logging level mapping tests ───────────────

#[test]
fn mcp_log_level_mapping_is_deterministic() {
    use rmcp::model::LoggingLevel;

    // Each MCP level maps to exactly one tracing level.
    let cases: &[(LoggingLevel, tracing::Level)] = &[
        (LoggingLevel::Debug, tracing::Level::TRACE),
        (LoggingLevel::Info, tracing::Level::DEBUG),
        (LoggingLevel::Notice, tracing::Level::INFO),
        (LoggingLevel::Warning, tracing::Level::WARN),
        (LoggingLevel::Error, tracing::Level::ERROR),
        (LoggingLevel::Critical, tracing::Level::ERROR),
        (LoggingLevel::Alert, tracing::Level::ERROR),
        (LoggingLevel::Emergency, tracing::Level::ERROR),
    ];

    for (mcp_level, expected_tracing_level) in cases {
        assert_eq!(
            mcp_log_level_to_tracing(*mcp_level),
            *expected_tracing_level,
            "mcp level {mcp_level:?} should map to {expected_tracing_level:?}"
        );
    }
}

#[test]
fn log_data_to_message_extracts_string_value() {
    assert_eq!(
        log_data_to_message(&serde_json::json!("hello world")),
        "hello world"
    );
}

#[test]
fn log_data_to_message_handles_null_value() {
    assert_eq!(log_data_to_message(&serde_json::json!(null)), "<null>");
}

#[test]
fn log_data_to_message_serializes_objects() {
    let obj = serde_json::json!({"key": "value", "count": 42});
    let msg = log_data_to_message(&obj);
    // Should produce valid JSON string
    let parsed: serde_json::Value = serde_json::from_str(&msg).expect("valid JSON");
    assert_eq!(parsed["key"], "value");
    assert_eq!(parsed["count"], 42);
}

#[test]
fn log_data_to_message_handles_arrays() {
    let arr = serde_json::json!([1, "two", 3]);
    let msg = log_data_to_message(&arr);
    let parsed: serde_json::Value = serde_json::from_str(&msg).expect("valid JSON");
    assert_eq!(parsed, serde_json::json!([1, "two", 3]));
}

#[test]
fn notification_handler_structures() {
    // Verify McpNotificationHandler can be constructed and cloned.
    let handler = McpNotificationHandler {
        server_name: "test-server".to_string(),
        task_short_id: "abc123".to_string(),
        routing: make_routing(HashMap::new(), HashMap::new()),
    };
    let clone = handler.clone();
    assert_eq!(clone.server_name, "test-server");
    assert_eq!(clone.task_short_id, "abc123");
}

/// Compile-time probe: `McpNotificationHandler` implements
/// `rmcp::ClientHandler`. This validates that:
/// - `on_logging_message` has the correct signature
/// - The handler is `Clone + Send + Sync + 'static` (required by ClientHandler)
/// - The handler can be used with `ServiceExt::serve` to connect to MCP servers
///
/// `NotificationContext` is `#[non_exhaustive]` in rmcp and `Peer::new` is
/// `pub(crate)`, so runtime invocation of `on_logging_message` requires the
/// full rmcp framework (transport handshake). The handler's notification
/// processing is tested indirectly through the `connect_to_server` integration
/// path and the level-mapping/message-extraction unit tests above.
#[test]
fn notification_handler_implements_client_handler() {
    fn assert_client_handler<T: rmcp::ClientHandler>() {}
    assert_client_handler::<McpNotificationHandler>();
}

#[test]
fn log_level_mapping_covers_all_mcp_variants() {
    use rmcp::model::LoggingLevel;

    // Verify that every MCP variant has a deterministic tracing mapping.
    // No variant should panic or fall through to a default.
    let all_variants = [
        LoggingLevel::Debug,
        LoggingLevel::Info,
        LoggingLevel::Notice,
        LoggingLevel::Warning,
        LoggingLevel::Error,
        LoggingLevel::Critical,
        LoggingLevel::Alert,
        LoggingLevel::Emergency,
    ];

    for variant in &all_variants {
        let tracing_level = mcp_log_level_to_tracing(*variant);
        // All levels must be one of the 5 standard tracing levels.
        assert!(
            matches!(
                tracing_level,
                tracing::Level::TRACE
                    | tracing::Level::DEBUG
                    | tracing::Level::INFO
                    | tracing::Level::WARN
                    | tracing::Level::ERROR
            ),
            "unexpected tracing level for {variant:?}: {tracing_level:?}"
        );
    }
}

#[test]
fn log_data_to_message_handles_number_values() {
    assert_eq!(log_data_to_message(&serde_json::json!(42)), "42");
    assert_eq!(log_data_to_message(&serde_json::json!(1.5)), "1.5");
}

#[test]
fn log_data_to_message_handles_boolean_values() {
    assert_eq!(log_data_to_message(&serde_json::json!(true)), "true");
    assert_eq!(log_data_to_message(&serde_json::json!(false)), "false");
}
