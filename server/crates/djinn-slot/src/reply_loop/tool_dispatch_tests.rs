// The telemetry guard mutex is intentionally held across awaits to serialize tests.
#![allow(clippy::await_holding_lock)]
use super::*;
use djinn_telemetry::render;
use std::sync::{Arc, Mutex, MutexGuard};
// `test_tool_schema`, `test_cancel_token`, and `turn_budget_telemetry_guard`
// are shared with the sibling `tool_dispatch_budget_tests` module (the
// turn-budget split), so they are `pub(super)` rather than module-private.
static TURN_BUDGET_TELEMETRY_MUTEX: Mutex<()> = Mutex::new(());
pub(super) fn turn_budget_telemetry_guard() -> MutexGuard<'static, ()> {
    TURN_BUDGET_TELEMETRY_MUTEX
        .lock()
        .expect("telemetry mutex poisoned")
}
pub(super) fn test_tool_schema(
    name: &str,
    read_only: Option<bool>,
    destructive: Option<bool>,
    idempotent: Option<bool>,
    open_world: Option<bool>,
    concurrent_safe: Option<bool>,
) -> serde_json::Value {
    let mut schema = serde_json::json!({
        "type": "function",
        "function": {
            "name": name,
            "description": "test",
            "parameters": {"type": "object"}
        }
    });
    let obj = schema.as_object_mut().expect("object schema");
    if let Some(value) = read_only {
        obj.insert("readOnly".to_string(), serde_json::Value::Bool(value));
    }
    if let Some(value) = destructive {
        obj.insert("destructive".to_string(), serde_json::Value::Bool(value));
    }
    if let Some(value) = idempotent {
        obj.insert("idempotent".to_string(), serde_json::Value::Bool(value));
    }
    if let Some(value) = open_world {
        obj.insert("openWorld".to_string(), serde_json::Value::Bool(value));
    }
    if let Some(value) = concurrent_safe {
        obj.insert(
            "concurrent_safe".to_string(),
            serde_json::Value::Bool(value),
        );
    }
    schema
}
struct PhaseScriptedDispatcher {
    clock: Arc<djinn_core::clock::TestClock>,
    retry_attempts: std::sync::atomic::AtomicUsize,
}
impl PhaseScriptedDispatcher {
    fn advance(&self, seconds: u64) {
        self.clock
            .advance_mono(std::time::Duration::from_secs(seconds));
    }
    fn route(&self, name: &str) -> Result<(), String> {
        let (seconds, error) = match name {
            "output_view" => (1, None),
            "output_grep" => (2, Some("stash error")),
            "mcp_ok" => (3, None),
            "mcp_err" => (4, Some("mcp error")),
            "resource_ok" => (5, None),
            "resource_err" => (6, Some("resource error")),
            "extension_ok" => (7, None),
            "extension_err" => (8, Some("extension error")),
            _ => unreachable!("unexpected scripted route: {name}"),
        };
        self.advance(seconds);
        error.map_or(Ok(()), |error| Err(error.into()))
    }
    fn json_future<'a>(
        &'a self,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    > {
        let result = self.route(name).map(|()| serde_json::json!({"ok": true}));
        Box::pin(async move { result })
    }
}
impl crate::host::SlotToolDispatcher for PhaseScriptedDispatcher {
    fn is_stash_tool(&self, name: &str) -> bool {
        matches!(name, "output_view" | "output_grep")
    }
    fn handle_stash_call(
        &self,
        name: &str,
        _: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<String, String> {
        self.route(name).map(|()| "stash ok".into())
    }
    fn render_result(&self, _: &str, _: &str, value: &serde_json::Value) -> String {
        value.to_string()
    }
    fn externalize_rendered_result(&self, _: &str, _: &str, rendered: &str, _: usize) -> String {
        rendered.to_string()
    }
    fn dispatch_extension_tool<'a>(
        &'a self,
        name: &'a str,
        _: Option<serde_json::Map<String, serde_json::Value>>,
        _: &'a std::path::Path,
        _: &'a str,
        _: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = djinn_core::tool_call::ToolCallOutcome> + Send + 'a>,
    > {
        match name {
            "extension_ok" | "extension_err" => {
                let result = self.route(name).map(|()| serde_json::json!({"ok": true}));
                Box::pin(async move { djinn_core::tool_call::ToolCallOutcome::from_result(result) })
            }
            "retry" => {
                self.advance(1);
                let attempt = self
                    .retry_attempts
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    djinn_core::tool_call::ToolCallOutcome::from_result(if attempt == 0 {
                        Err("database is locked".into())
                    } else {
                        Ok(serde_json::json!({"retried": true}))
                    })
                })
            }
            "pending" => Box::pin(std::future::pending()),
            _ => unreachable!("unexpected extension tool: {name}"),
        }
    }
    fn is_mcp_tool(&self, name: &str) -> bool {
        matches!(name, "mcp_ok" | "mcp_err")
    }
    fn dispatch_mcp_tool<'a>(
        &'a self,
        name: &'a str,
        _: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send + 'a>,
    > {
        self.json_future(name)
    }
    fn mcp_server_for_tool(&self, _: &str) -> Option<String> {
        None
    }
    fn is_resource_tool(&self, name: &str) -> bool {
        matches!(name, "resource_ok" | "resource_err")
    }
    fn dispatch_resource_tool<'a>(
        &'a self,
        name: &'a str,
        _: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        let result = self.route(name).map(|()| "resource ok".into());
        Box::pin(async move { result })
    }
    fn clear_stash(&self) {}
}
fn phase_metric_value(rendered: &str) -> f64 {
    rendered
        .lines()
        .find(|line| {
            line.starts_with("djinn_agent_session_phase_seconds_total")
                && line.contains("phase=\"tool_execution\"")
                && line.contains("role=\"worker\"")
        })
        .and_then(|line| line.rsplit_once(' ').map(|(_, value)| value))
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("missing worker tool_execution sample in:\n{rendered}"))
}
fn assert_phase_labels_are_bounded(rendered: &str) {
    for line in rendered
        .lines()
        .filter(|line| line.starts_with("djinn_agent_session_phase_seconds_total{"))
    {
        let labels = line
            .split_once('{')
            .and_then(|(_, tail)| tail.split_once('}'))
            .map(|(labels, _)| labels)
            .expect("phase metric must have labels");
        assert!(
            labels.split(',').all(|label| {
                matches!(
                    label,
                    "phase=\"provider_wait\""
                        | "phase=\"tool_execution\""
                        | "role=\"worker\""
                        | "role=\"reviewer\""
                        | "role=\"planner\""
                        | "role=\"refinement\""
                )
            }),
            "bad phase label"
        );
    }
}
fn scripted_phase_context() -> (
    SlotContext,
    Arc<djinn_core::clock::TestClock>,
    Arc<Mutex<SessionPhaseTracker>>,
    ToolRuntimeMetadataMap,
) {
    use crate::test_helpers::{agent_context_from_db_with_dispatcher, create_test_db};
    use std::time::{Instant, SystemTime};
    use tokio_util::sync::CancellationToken;
    let clock = Arc::new(djinn_core::clock::TestClock::new(
        SystemTime::UNIX_EPOCH,
        Instant::now(),
    ));
    let dispatcher = Arc::new(PhaseScriptedDispatcher {
        clock: Arc::clone(&clock),
        retry_attempts: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut ctx = agent_context_from_db_with_dispatcher(
        create_test_db(),
        CancellationToken::new(),
        Some(dispatcher),
    );
    ctx.clock = clock.clone();
    let tracker = Arc::new(Mutex::new(SessionPhaseTracker::new(&ctx, "worker")));
    (ctx, clock, tracker, ToolRuntimeMetadataMap::new())
}
fn scripted_request(idx: usize, name: &str, retry_safe: bool) -> ToolDispatchRequest {
    ToolDispatchRequest {
        idx,
        id: format!("call-{name}"),
        name: name.into(),
        args: None,
        tool_span: None,
        retry_safe,
    }
}
#[tokio::test]
async fn dispatcher_phase_metrics_cover_success_and_returned_errors_for_all_routes() {
    let _guard = turn_budget_telemetry_guard();
    djinn_telemetry::init().expect("telemetry init");
    let (ctx, _clock, tracker, metadata) = scripted_phase_context();
    let dispatch =
        test_tracked_dispatch_context(&ctx, &metadata, std::path::Path::new("/tmp"), &tracker);
    let before = phase_metric_value(&render().expect("render metrics"));
    for (idx, name) in [
        "output_view",
        "output_grep",
        "mcp_ok",
        "mcp_err",
        "resource_ok",
        "resource_err",
        "extension_ok",
        "extension_err",
    ]
    .into_iter()
    .enumerate()
    {
        let (_, result) = dispatch_single_tool(scripted_request(idx, name, false), &dispatch).await;
        let ContentBlock::ToolResult { is_error, .. } = result else {
            panic!("bad result")
        };
        assert_eq!(is_error, name.ends_with("err") || name == "output_grep");
    }
    let rendered = render().expect("render metrics after dispatches");
    assert_eq!(phase_metric_value(&rendered) - before, 36.0);
    assert_phase_labels_are_bounded(&rendered);
}
#[tokio::test(start_paused = true)]
async fn extension_retry_and_backoff_are_one_outer_tool_interval() {
    let _guard = turn_budget_telemetry_guard();
    djinn_telemetry::init().expect("telemetry init");
    let (ctx, clock, tracker, metadata) = scripted_phase_context();
    let dispatch =
        test_tracked_dispatch_context(&ctx, &metadata, std::path::Path::new("/tmp"), &tracker);
    let before = phase_metric_value(&render().expect("render metrics"));
    let future = dispatch_single_tool(scripted_request(0, "retry", true), &dispatch);
    tokio::pin!(future);
    assert!(
        futures::poll!(&mut future).is_pending(),
        "retry backoff must be pending"
    );
    clock.advance_mono(std::time::Duration::from_secs(2));
    tokio::time::advance(std::time::Duration::from_millis(200)).await;
    let (_, result) = future.await;
    assert!(matches!(
        result,
        ContentBlock::ToolResult {
            is_error: false,
            ..
        }
    ));
    let rendered = render().expect("render metrics after retry");
    assert_eq!(phase_metric_value(&rendered) - before, 4.0);
}
#[tokio::test]
async fn concurrent_dispatch_guards_suppress_nested_tool_intervals_and_drop_flushes_once() {
    let _guard = turn_budget_telemetry_guard();
    djinn_telemetry::init().expect("telemetry init");
    let (ctx, clock, tracker, metadata) = scripted_phase_context();
    let dispatch =
        test_tracked_dispatch_context(&ctx, &metadata, std::path::Path::new("/tmp"), &tracker);
    let before = phase_metric_value(&render().expect("render metrics"));
    {
        let first = dispatch_single_tool(scripted_request(0, "pending", false), &dispatch);
        let second = dispatch_single_tool(scripted_request(1, "pending", false), &dispatch);
        tokio::pin!(first);
        tokio::pin!(second);
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut second).is_pending());
        clock.advance_mono(std::time::Duration::from_secs(7));
    }
    let rendered = render().expect("render after cancellation");
    assert_eq!(phase_metric_value(&rendered) - before, 7.0);
    assert_phase_labels_are_bounded(&rendered);
}
pub(super) fn test_cancel_token() -> &'static tokio_util::sync::CancellationToken {
    static TOKEN: std::sync::OnceLock<tokio_util::sync::CancellationToken> =
        std::sync::OnceLock::new();
    TOKEN.get_or_init(tokio_util::sync::CancellationToken::new)
}
fn test_tracked_dispatch_context<'a>(
    ctx: &'a SlotContext,
    tool_metadata: &'a ToolRuntimeMetadataMap,
    worktree_path: &'a std::path::Path,
    phase_tracker: &'a Arc<Mutex<SessionPhaseTracker>>,
) -> ToolDispatchContext<'a> {
    ToolDispatchContext {
        ctx,
        task_id: "test-task",
        worktree_path,
        role_name: "worker",
        tool_metadata,
        tool_dispatcher: ctx.tool_dispatcher.as_ref().unwrap().as_ref(),
        otel_session: None,
        phase_tracker: Some(phase_tracker),
        cancel: test_cancel_token(),
    }
}
#[test]
fn runtime_metadata_parses_safety_annotations_and_gates_retry() {
    let schemas = vec![
        test_tool_schema(
            "safe_read",
            Some(true),
            Some(false),
            Some(true),
            Some(false),
            Some(true),
        ),
        test_tool_schema(
            "open_read",
            Some(true),
            Some(false),
            Some(true),
            Some(true),
            Some(true),
        ),
        test_tool_schema(
            "idempotent_write",
            Some(false),
            Some(false),
            Some(true),
            Some(false),
            Some(false),
        ),
        test_tool_schema(
            "non_idempotent_write",
            Some(false),
            Some(false),
            Some(false),
            Some(false),
            Some(false),
        ),
        test_tool_schema(
            "destructive",
            Some(false),
            Some(true),
            Some(true),
            Some(false),
            Some(false),
        ),
        test_tool_schema("missing_metadata", None, None, None, None, None),
    ];
    let metadata = tool_runtime_metadata(&schemas);
    assert_eq!(
        metadata["open_read"],
        ToolRuntimeMetadata {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: true,
            concurrent_safe: true,
        }
    );
    assert!(is_side_query_tool(&metadata, "safe_read"));
    assert!(is_side_query_tool(&metadata, "open_read"));
    assert!(is_tool_retry_safe(&metadata, "safe_read"));
    assert!(is_tool_retry_safe(&metadata, "open_read"));
    assert!(is_tool_retry_safe(&metadata, "idempotent_write"));
    assert!(!is_side_query_tool(&metadata, "idempotent_write"));
    assert!(!is_side_query_tool(&metadata, "non_idempotent_write"));
    assert!(!is_tool_retry_safe(&metadata, "non_idempotent_write"));
    assert!(!is_side_query_tool(&metadata, "destructive"));
    assert!(!is_tool_retry_safe(&metadata, "destructive"));
    assert!(!is_side_query_tool(&metadata, "missing_metadata"));
    assert!(!is_tool_retry_safe(&metadata, "missing_metadata"));
    assert!(!is_side_query_tool(&metadata, "unknown"));
    assert!(!is_tool_retry_safe(&metadata, "unknown"));
}
#[tokio::test(start_paused = true)]
async fn heartbeat_fires_while_a_long_tool_runs() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let beats = AtomicU32::new(0);
    let interval = std::time::Duration::from_secs(30);
    let out = run_with_heartbeat(
        interval,
        || async {
            beats.fetch_add(1, Ordering::SeqCst);
        },
        async {
            tokio::time::sleep(std::time::Duration::from_secs(95)).await;
            42u32
        },
    )
    .await;
    assert_eq!(out, 42);
    assert_eq!(beats.load(Ordering::SeqCst), 3, "bad heartbeat count");
}
#[tokio::test(start_paused = true)]
async fn heartbeat_does_not_fire_for_a_fast_tool() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let beats = AtomicU32::new(0);
    let out = run_with_heartbeat(
        std::time::Duration::from_secs(30),
        || async {
            beats.fetch_add(1, Ordering::SeqCst);
        },
        async { 7u32 },
    )
    .await;
    assert_eq!(out, 7);
    assert_eq!(beats.load(Ordering::SeqCst), 0);
}
