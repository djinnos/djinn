use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn serve_one_sse_response(payload: &'static str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = socket.read(&mut request).await.unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });
    format!("http://{address}")
}

fn assert_unexpected_eof(error: &anyhow::Error, expected_request_bytes: usize) {
    match error.downcast_ref::<ProviderError>() {
        Some(ProviderError::ExhaustedTransport(diagnostic)) => {
            assert_eq!(
                diagnostic.category,
                super::super::transport::ExhaustedTransportCategory::UnexpectedEof
            );
            assert_eq!(diagnostic.estimated_payload_chars, expected_request_bytes);
        }
        other => panic!("expected exhausted UnexpectedEof transport error, got {other:?}"),
    }
}

#[tokio::test]
async fn stream_sse_empty_body_reports_unexpected_eof_with_exact_request_bytes() {
    let url = serve_one_sse_response("").await;
    let body = serde_json::json!({"model": "fixture", "message": "é"});
    let expected_request_bytes = serde_json::to_vec(&body).unwrap().len();
    let mut stream = ApiClient::new().stream_sse(&url, body, &AuthMethod::NoAuth, HeaderMap::new());

    let error = stream.next().await.unwrap().unwrap_err();
    assert_unexpected_eof(&error, expected_request_bytes);
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn stream_sse_frames_empty_body_reports_unexpected_eof_with_exact_request_bytes() {
    let url = serve_one_sse_response("").await;
    let body = serde_json::json!({"model": "fixture", "message": "éé"});
    let expected_request_bytes = serde_json::to_vec(&body).unwrap().len();
    let mut stream =
        ApiClient::new().stream_sse_frames(&url, body, &AuthMethod::NoAuth, HeaderMap::new());

    let error = stream.next().await.unwrap().unwrap_err();
    assert_unexpected_eof(&error, expected_request_bytes);
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn stream_sse_eof_after_data_remains_an_ordinary_end_of_stream() {
    let url = serve_one_sse_response("data: {\"ok\":true}\n\n").await;
    let body = serde_json::json!({"model": "fixture"});
    let mut stream = ApiClient::new().stream_sse(&url, body, &AuthMethod::NoAuth, HeaderMap::new());

    assert_eq!(
        stream.next().await.unwrap().unwrap(),
        "{\"ok\":true}".to_string()
    );
    assert!(stream.next().await.is_none());
}

#[test]
fn request_timeout_is_10_minutes() {
    assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(600));
}

#[test]
fn parse_sse_data_line_handles_space_and_no_space() {
    // Anthropic/OpenAI form (space after colon).
    assert_eq!(
        parse_sse_data_line("data: {\"type\":\"message_start\"}"),
        Some("{\"type\":\"message_start\"}")
    );
    // Kimi for Coding form (NO space after colon) — the regression.
    assert_eq!(
        parse_sse_data_line("data:{\"type\":\"message_start\"}"),
        Some("{\"type\":\"message_start\"}")
    );
}

#[test]
fn parse_sse_data_line_skips_non_data_and_sentinels() {
    // Non-data SSE fields and comments are ignored.
    assert_eq!(parse_sse_data_line("event:message_start"), None);
    assert_eq!(parse_sse_data_line("event: message_start"), None);
    assert_eq!(parse_sse_data_line("id: 42"), None);
    assert_eq!(parse_sse_data_line(": keep-alive comment"), None);
    assert_eq!(parse_sse_data_line(""), None);
    // Empty payloads and the [DONE] sentinel (either spacing) are dropped.
    assert_eq!(parse_sse_data_line("data:"), None);
    assert_eq!(parse_sse_data_line("data: "), None);
    assert_eq!(parse_sse_data_line("data: [DONE]"), None);
    assert_eq!(parse_sse_data_line("data:[DONE]"), None);
}

#[test]
fn classify_sse_line_surfaces_done_sentinel() {
    // Unlike parse_sse_data_line which swallows [DONE], classify_sse_line
    // surfaces it as SseFrame::Done for adapter-level terminal tracking.
    assert_eq!(classify_sse_line("data: [DONE]"), Some(SseFrame::Done));
    assert_eq!(classify_sse_line("data:[DONE]"), Some(SseFrame::Done));
}

#[test]
fn classify_sse_line_data_payloads() {
    assert_eq!(
        classify_sse_line("data: {\"type\":\"message_start\"}"),
        Some(SseFrame::Data("{\"type\":\"message_start\"}".to_string()))
    );
    assert_eq!(
        classify_sse_line("data:{\"type\":\"message_start\"}"),
        Some(SseFrame::Data("{\"type\":\"message_start\"}".to_string()))
    );
}

#[test]
fn classify_sse_line_skips_non_data_and_empty() {
    assert_eq!(classify_sse_line("event: message_start"), None);
    assert_eq!(classify_sse_line("id: 42"), None);
    assert_eq!(classify_sse_line(": keep-alive comment"), None);
    assert_eq!(classify_sse_line(""), None);
    // Empty data payloads are skipped (not SseFrame::Done).
    assert_eq!(classify_sse_line("data:"), None);
    assert_eq!(classify_sse_line("data: "), None);
}

// ── STREAM_CHUNK_TIMEOUT invariants ───────────────────────────────────────────
//
// The transport/chunk read timeout must be at least the longest reasoning-family
// first-event floor so the TTFT detector is never capped below its intended
// budget. The reasoning floor is 600s (see
// `crate::provider::first_event::REASONING_FLOOR_TIMEOUT`). These unit tests
// sit on `STREAM_CHUNK_TIMEOUT` directly rather than going through
// `first_event_budget` so a future regression that bumps the floor without
// raising the chunk timeout is caught here at compile+test time.
#[test]
fn stream_chunk_timeout_at_least_reasoning_floor() {
    assert!(
        STREAM_CHUNK_TIMEOUT >= crate::provider::first_event::REASONING_FLOOR_TIMEOUT,
        "STREAM_CHUNK_TIMEOUT ({:?}) must be >= REASONING_FLOOR_TIMEOUT ({:?})",
        STREAM_CHUNK_TIMEOUT,
        crate::provider::first_event::REASONING_FLOOR_TIMEOUT
    );
    assert_eq!(STREAM_CHUNK_TIMEOUT, Duration::from_secs(600));
}

#[test]
fn stream_chunk_timeout_is_larger_than_non_reasoning_default() {
    // The chunk timeout must also exceed the non-reasoning default
    // first-event budget (90s) — otherwise every non-reasoning stream
    // would hit the chunk timeout before the TTFT guard could fire.
    assert!(
        STREAM_CHUNK_TIMEOUT > crate::provider::first_event::DEFAULT_FIRST_EVENT_TIMEOUT,
        "STREAM_CHUNK_TIMEOUT ({:?}) must be > DEFAULT_FIRST_EVENT_TIMEOUT ({:?})",
        STREAM_CHUNK_TIMEOUT,
        crate::provider::first_event::DEFAULT_FIRST_EVENT_TIMEOUT
    );
}

#[test]
fn first_event_budget_non_reasoning_is_within_90s() {
    // The non-reasoning default budget must be no more than 90s.
    assert!(
        first_event_budget("gpt-4o", None) <= Duration::from_secs(90),
        "default non-reasoning budget must be <= 90s"
    );
}

#[test]
fn first_event_budget_reasoning_floor_is_600s() {
    // Reasoning families get a ~600s floor via start-anchored matching.
    assert_eq!(first_event_budget("o1", None), Duration::from_secs(600));
    assert_eq!(
        first_event_budget("gpt-5.1-codex", None),
        Duration::from_secs(600)
    );
}

#[test]
fn first_event_budget_explicit_config_overrides_floor() {
    // Explicit config always wins, even when lower than the floor.
    assert_eq!(
        first_event_budget("o1", Some(Duration::from_secs(30))),
        Duration::from_secs(30)
    );
}

#[test]
fn first_event_budget_floor_does_not_lower_explicit() {
    // A floor must never lower an explicit threshold.
    assert_eq!(
        first_event_budget("gpt-4o", Some(Duration::from_secs(120))),
        Duration::from_secs(120)
    );
}

#[test]
fn backoff_delay_first_attempt() {
    let delay = backoff_delay_ms(1);
    // First attempt: 1000ms * 2^0 = 1000ms, with 0.8-1.2x jitter
    assert!((800..=1200).contains(&delay), "delay was {delay}");
}

#[test]
fn backoff_delay_representative_attempts_stay_within_jitter_window() {
    for attempt in [2, 3, 5] {
        let delay = backoff_delay_ms(attempt);
        let base = (INITIAL_BACKOFF_MS as f64 * BACKOFF_MULTIPLIER.powi(attempt as i32 - 1)) as u64;
        let min = (base as f64 * 0.8) as u64;
        let max = (base as f64 * 1.2) as u64;
        assert!(
            (min..=max).contains(&delay),
            "attempt {attempt} delay was {delay}"
        );
    }
}

#[test]
fn backoff_delay_capped_at_max() {
    let delay = backoff_delay_ms(100);
    // Should be capped at MAX_BACKOFF_MS (30s) * 1.2x jitter max
    assert!(
        delay <= ((MAX_BACKOFF_MS as f64 * 1.2) as u64),
        "delay was {delay}"
    );
}

#[test]
fn retryable_status_policy_matches_expectations() {
    assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
    assert!(is_retryable_status(
        reqwest::StatusCode::INTERNAL_SERVER_ERROR
    ));
    assert!(is_retryable_status(
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    ));

    assert!(!is_retryable_status(reqwest::StatusCode::BAD_REQUEST));
    assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
    assert!(!is_retryable_status(reqwest::StatusCode::NOT_FOUND));
}

#[test]
fn retry_budget_allows_only_max_retries_after_initial_attempt() {
    assert!(should_retry(0, true));
    assert!(should_retry(1, true));
    assert!(should_retry(2, true));
    assert!(!should_retry(MAX_RETRIES, true));
    assert!(!should_retry(MAX_RETRIES + 1, true));
    assert!(!should_retry(0, false));
}

#[test]
fn client_builds_successfully() {
    let _client = ApiClient::new();
}

#[test]
fn retry_after_ms_parses_seconds_and_http_dates() {
    let mut seconds = HeaderMap::new();
    seconds.insert("retry-after", "7".parse().unwrap());
    assert_eq!(retry_after_ms(&seconds), Some(7000));

    let mut http_date = HeaderMap::new();
    let future = (time::OffsetDateTime::now_utc() + time::Duration::seconds(2))
        .format(&time::format_description::well_known::Rfc2822)
        .unwrap();
    http_date.insert("retry-after", future.parse().unwrap());
    assert!(retry_after_ms(&http_date).is_some_and(|value| value > 0));
}

#[test]
fn rate_limit_status_matches_429_and_529_only() {
    assert!(is_rate_limit_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
    assert!(is_rate_limit_status(
        reqwest::StatusCode::from_u16(529).unwrap()
    ));
    assert!(!is_rate_limit_status(
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    ));
}

// ─── Outbound-request debug logger (DJINN_DEBUG_PROVIDER_REQUEST) ─────────

use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;

/// Serialize tests that mutate the process-global env var + tracing default.
fn env_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

#[derive(Clone, Default)]
struct CapturedEvents(Arc<StdMutex<Vec<String>>>);

struct CaptureVisitor<'a>(&'a mut Vec<String>);
impl Visit for CaptureVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push(format!("{}={:?}", field.name(), value));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push(format!("{}={}", field.name(), value));
    }
}

impl<S: tracing::Subscriber> Layer<S> for CapturedEvents {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if event.metadata().target() != OUTBOUND_REQUEST_TARGET {
            return;
        }
        let mut fields = Vec::new();
        event.record(&mut CaptureVisitor(&mut fields));
        self.0.lock().unwrap().push(fields.join(" | "));
    }
}

fn capture_with_env(value: Option<&str>) -> Vec<String> {
    let captured = CapturedEvents(Arc::new(StdMutex::new(Vec::new())));
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    tracing::subscriber::with_default(subscriber, || {
        match value {
            Some(v) => unsafe { std::env::set_var(DEBUG_PROVIDER_REQUEST_ENV, v) },
            None => unsafe { std::env::remove_var(DEBUG_PROVIDER_REQUEST_ENV) },
        }
        let body = serde_json::json!({
            "model": "kimi-for-coding/k2p7",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let auth = AuthMethod::BearerToken("sk-super-secret-token".to_string());
        let mut headers = HeaderMap::new();
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        log_outbound_request(
            "POST",
            "https://api.kimi.com/coding/v1",
            &body,
            &auth,
            &headers,
        );
    });
    captured.0.lock().unwrap().clone()
}

#[test]
fn logger_silent_when_env_unset() {
    let _g = env_test_guard();
    let events = capture_with_env(None);
    assert!(
        events.is_empty(),
        "no outbound-request event must be emitted when {DEBUG_PROVIDER_REQUEST_ENV} is unset: {events:?}"
    );
    // A falsey value is also OFF.
    let events = capture_with_env(Some("0"));
    assert!(
        events.is_empty(),
        "falsey value must keep the logger OFF: {events:?}"
    );
}

#[test]
fn logger_emits_and_redacts_when_env_set() {
    let _g = env_test_guard();
    for truthy in ["1", "true", "YES", "on"] {
        let events = capture_with_env(Some(truthy));
        assert_eq!(
            events.len(),
            1,
            "exactly one outbound-request event expected for {truthy:?}: {events:?}"
        );
        let joined = &events[0];
        // The token must never appear in the captured event — redacted in
        // both the Authorization header and (defensively) the body.
        assert!(
            !joined.contains("sk-super-secret-token"),
            "secret token leaked into log for {truthy:?}: {joined}"
        );
        assert!(
            joined.contains("***REDACTED***"),
            "expected redaction marker: {joined}"
        );
        // Useful fields are present for diffing against a known-good curl.
        assert!(
            joined.contains("kimi-for-coding/k2p7"),
            "model id field missing: {joined}"
        );
        assert!(
            joined.contains("https://api.kimi.com/coding/v1"),
            "url field missing: {joined}"
        );
        assert!(
            joined.contains("Authorization"),
            "auth header missing: {joined}"
        );
        assert!(
            joined.contains("anthropic-version"),
            "extra header missing: {joined}"
        );
    }
    // Cleanup so the env var never leaks to a sibling test.
    unsafe { std::env::remove_var(DEBUG_PROVIDER_REQUEST_ENV) };
}
