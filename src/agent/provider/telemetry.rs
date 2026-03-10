//! OpenTelemetry instrumentation for LLM provider calls.
//!
//! Sends trace data to a Langfuse-compatible OTLP endpoint. LLM requests go
//! directly to the provider — no proxy involved. Trace spans are emitted
//! asynchronously via the OpenTelemetry SDK batch processor.
//!
//! Span hierarchy:
//!   SessionSpan (root trace — one per reply loop)
//!     └─ LlmSpan (generation — one per LLM turn)
//!     └─ ToolSpan (tool call — one per tool invocation)

use opentelemetry::trace::{SpanKind, Status, TraceContextExt, Tracer};
use opentelemetry::{Context, KeyValue, global};
use opentelemetry_otlp::{SpanExporter, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::sync::OnceLock;

/// Langfuse/OTLP configuration loaded from DjinnSettings.
#[derive(Clone, Debug)]
pub struct LangfuseConfig {
    /// OTLP endpoint (e.g. `http://localhost:3000/api/public/otel`).
    pub endpoint: String,
    /// Langfuse public key (used as OTLP username via Basic Auth).
    pub public_key: String,
    /// Langfuse secret key (used as OTLP password via Basic Auth).
    pub secret_key: String,
}

impl LangfuseConfig {
    /// Build the Basic Auth header value: base64("pk:sk").
    fn auth_header(&self) -> String {
        use base64::Engine;
        let credentials = format!("{}:{}", self.public_key, self.secret_key);
        let encoded = base64::engine::general_purpose::STANDARD.encode(credentials.as_bytes());
        format!("Basic {encoded}")
    }
}

/// Global flag indicating whether telemetry is active.
static TELEMETRY_ACTIVE: OnceLock<bool> = OnceLock::new();
/// Store the provider for graceful shutdown.
static TRACER_PROVIDER: OnceLock<SdkTracerProvider> = OnceLock::new();

/// Returns true if OTel telemetry has been initialized.
pub fn is_active() -> bool {
    TELEMETRY_ACTIVE.get().copied().unwrap_or(false)
}

/// Initialize the OpenTelemetry tracer provider with Langfuse OTLP export.
///
/// Call once at startup. Safe to call multiple times — subsequent calls are
/// no-ops. Returns `Ok(true)` if initialized, `Ok(false)` if already active.
pub fn init(config: &LangfuseConfig) -> anyhow::Result<bool> {
    if is_active() {
        return Ok(false);
    }

    let mut headers = std::collections::HashMap::new();
    headers.insert("Authorization".to_string(), config.auth_header());

    let exporter = SpanExporter::builder()
        .with_http()
        .with_http_client(
            std::thread::spawn(|| reqwest::blocking::Client::new())
                .join()
                .expect("failed to build blocking reqwest client"),
        )
        .with_endpoint(format!("{}/v1/traces", config.endpoint.trim_end_matches('/')))
        .with_headers(headers)
        .build()?;

    let resource = Resource::builder()
        .with_service_name("djinn-server")
        .build();

    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build();

    let _ = TRACER_PROVIDER.set(provider.clone());
    global::set_tracer_provider(provider);
    let _ = TELEMETRY_ACTIVE.set(true);

    tracing::info!(
        endpoint = %config.endpoint,
        "OpenTelemetry tracer initialized (Langfuse OTLP)"
    );

    Ok(true)
}

/// Flush pending spans and shut down the tracer provider.
/// Call during graceful shutdown.
pub fn shutdown() {
    if let Some(provider) = TRACER_PROVIDER.get() {
        if let Err(e) = provider.shutdown() {
            tracing::warn!(error = %e, "OpenTelemetry tracer shutdown error");
        } else {
            tracing::info!("OpenTelemetry tracer shut down");
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

const TRACER_NAME: &str = "djinn-llm";

/// Truncate a string to a max byte length, appending "...(truncated)" if cut.
fn truncate(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        s.to_string()
    } else {
        format!("{}...(truncated)", &s[..max_bytes])
    }
}

// ─── Session span (root trace) ───────────────────────────────────────────────

/// Attributes for the root session span.
pub struct SessionSpanAttributes<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    pub task_short_id: &'a str,
    pub task_id: &'a str,
    pub agent_type: &'a str,
    pub session_id: &'a str,
}

/// Root span for an entire reply loop session. All LLM generation and tool
/// call spans are created as children of this span, so Langfuse groups them
/// into a single trace.
///
/// The inner span lives inside a `Context` and is accessed via `SpanRef`
/// (interior mutability through `Mutex`).
pub struct SessionSpan {
    cx: Context,
}

impl SessionSpan {
    /// Start a new session-level root span.
    pub fn start(attrs: &SessionSpanAttributes<'_>) -> Self {
        let tracer = global::tracer(TRACER_NAME);

        let span_name = format!("{}:{}", attrs.agent_type, attrs.task_short_id);

        let span = tracer
            .span_builder(span_name)
            .with_kind(SpanKind::Server)
            .with_attributes(vec![
                KeyValue::new("gen_ai.system", attrs.provider.to_string()),
                KeyValue::new("gen_ai.request.model", attrs.model.to_string()),
                KeyValue::new("langfuse.session.id", attrs.session_id.to_string()),
                KeyValue::new(
                    "langfuse.trace.name",
                    format!("{}:{}", attrs.agent_type, attrs.task_short_id),
                ),
                KeyValue::new("djinn.task_id", attrs.task_id.to_string()),
                KeyValue::new("djinn.agent_type", attrs.agent_type.to_string()),
            ])
            .start(&tracer);

        let cx = Context::current_with_span(span);

        Self { cx }
    }

    /// Record the system prompt on the session trace.
    pub fn record_system_prompt(&self, prompt: &str) {
        self.cx.span().set_attribute(KeyValue::new(
            "gen_ai.system_prompt",
            truncate(prompt, 20_000),
        ));
    }

    /// Record cumulative token usage on the session trace.
    pub fn record_usage(&self, input_tokens: u32, output_tokens: u32) {
        let span = self.cx.span();
        span.set_attribute(KeyValue::new(
            "gen_ai.usage.input_tokens",
            input_tokens as i64,
        ));
        span.set_attribute(KeyValue::new(
            "gen_ai.usage.output_tokens",
            output_tokens as i64,
        ));
        span.set_attribute(KeyValue::new(
            "gen_ai.usage.total_tokens",
            (input_tokens + output_tokens) as i64,
        ));
    }

    /// Get the context for creating child spans.
    pub fn context(&self) -> &Context {
        &self.cx
    }

    /// Mark the session as successful and end the span.
    pub fn end_ok(self) {
        self.cx.span().set_status(Status::Ok);
        self.cx.span().end();
    }

    /// Mark the session as failed and end the span.
    pub fn end_error(self, error: &str) {
        self.cx
            .span()
            .set_status(Status::error(error.to_string()));
        self.cx.span().end();
    }
}

// ─── LLM generation span (child of session) ─────────────────────────────────

/// An active LLM generation span. Created as a child of a SessionSpan.
pub struct LlmSpan {
    cx: Context,
}

impl LlmSpan {
    /// Start a new LLM generation span as a child of the given session context.
    pub fn start(parent_cx: &Context, provider: &str, model: &str, turn: u32) -> Self {
        let tracer = global::tracer(TRACER_NAME);

        let span = tracer
            .span_builder(format!("generation.{turn}"))
            .with_kind(SpanKind::Client)
            .with_attributes(vec![
                KeyValue::new("gen_ai.system", provider.to_string()),
                KeyValue::new("gen_ai.request.model", model.to_string()),
                KeyValue::new("langfuse.observation.type", "generation"),
            ])
            .start_with_context(&tracer, parent_cx);

        let cx = parent_cx.with_span(span);

        Self { cx }
    }

    /// Record the input prompt (last user message) on the span.
    pub fn record_input(&self, input: &str) {
        self.cx
            .span()
            .set_attribute(KeyValue::new("gen_ai.prompt", truncate(input, 10_000)));
    }

    /// Record the LLM completion output on the span.
    pub fn record_output(&self, output: &str) {
        self.cx
            .span()
            .set_attribute(KeyValue::new("gen_ai.completion", truncate(output, 10_000)));
    }

    /// Record token usage on the span.
    pub fn record_usage(&self, input_tokens: u32, output_tokens: u32) {
        let span = self.cx.span();
        span.set_attribute(KeyValue::new(
            "gen_ai.usage.input_tokens",
            input_tokens as i64,
        ));
        span.set_attribute(KeyValue::new(
            "gen_ai.usage.output_tokens",
            output_tokens as i64,
        ));
        span.set_attribute(KeyValue::new(
            "gen_ai.usage.total_tokens",
            (input_tokens + output_tokens) as i64,
        ));
    }

    /// Record tool calls made in this generation turn.
    pub fn record_tool_calls(&self, tool_names: &[String]) {
        if !tool_names.is_empty() {
            self.cx.span().set_attribute(KeyValue::new(
                "gen_ai.tool_calls",
                tool_names.join(", "),
            ));
        }
    }

    /// Mark the span as successful and end it.
    pub fn end_ok(self) {
        self.cx.span().set_status(Status::Ok);
        self.cx.span().end();
    }

    /// Mark the span as failed with an error message and end it.
    pub fn end_error(self, error: &str) {
        self.cx
            .span()
            .set_status(Status::error(error.to_string()));
        self.cx.span().end();
    }
}

// ─── Tool call span (child of session) ───────────────────────────────────────

/// A span representing a single tool call execution.
pub struct ToolSpan {
    cx: Context,
}

impl ToolSpan {
    /// Start a tool call span as a child of the session context.
    pub fn start(parent_cx: &Context, tool_name: &str, tool_use_id: &str) -> Self {
        let tracer = global::tracer(TRACER_NAME);

        let span = tracer
            .span_builder(format!("tool.{tool_name}"))
            .with_kind(SpanKind::Internal)
            .with_attributes(vec![
                KeyValue::new("langfuse.observation.type", "span"),
                KeyValue::new("tool.name", tool_name.to_string()),
                KeyValue::new("tool.use_id", tool_use_id.to_string()),
            ])
            .start_with_context(&tracer, parent_cx);

        let cx = parent_cx.with_span(span);

        Self { cx }
    }

    /// Record the tool call input arguments.
    pub fn record_input(&self, input: &str) {
        self.cx
            .span()
            .set_attribute(KeyValue::new("tool.input", truncate(input, 10_000)));
    }

    /// Record the tool call result.
    pub fn record_output(&self, output: &str, is_error: bool) {
        self.cx
            .span()
            .set_attribute(KeyValue::new("tool.output", truncate(output, 10_000)));
        if is_error {
            self.cx
                .span()
                .set_attribute(KeyValue::new("tool.is_error", true));
        }
    }

    /// End the tool span as successful.
    pub fn end_ok(self) {
        self.cx.span().set_status(Status::Ok);
        self.cx.span().end();
    }

    /// End the tool span as failed.
    pub fn end_error(self, error: &str) {
        self.cx
            .span()
            .set_status(Status::error(error.to_string()));
        self.cx.span().end();
    }
}
