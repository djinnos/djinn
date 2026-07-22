//! Focused integration tests for the provider transport's first-event (TTFT)
//! guard.
//!
//! The guard fires inside `ApiClient::stream_sse` after a successful HTTP
//! response, when no usable SSE `data:` event lands before the derived
//! detector budget. The production budgets are 90s (non-reasoning) and 600s
//! (reasoning floor), so we cannot wait for them in tests; instead we use
//! `ApiClient::new_with_first_event_budget_override` to shrink the budget
//! to a few hundred milliseconds.
//!
//! To exercise the guard we need a server that returns a successful 200 SSE
//! response (headers only) and then keeps the connection open without
//! delivering any `data:` event. The wiremock crate cannot model that
//! (it always flushes the response body together), so the tests below use a
//! tiny in-process TCP server built on `tokio::net::TcpListener` that
//! writes the response headers manually and then parks the connection.
//!
//! Acceptance-criterion coverage:
//!
//! - AC1: a successful HTTP response that never delivers a usable `data:`
//!   event must surface as a typed exhausted-transport diagnostic (retryable,
//!   feeds failover), not an empty turn.
//! - AC4: a focused async/client test exercises the actual first-event
//!   timeout behavior end-to-end.
//!
//! Adapter semantics are deliberately untouched here: we assert on the
//! typed error returned by the transport, NOT
//! on any `[DONE]`-style terminal handling.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use djinn_provider::provider::AuthMethod;
use djinn_provider::provider::client::ApiClient;
use djinn_provider::provider::error::ProviderError;
use djinn_provider::provider::{ExhaustedTransportCategory, ExhaustedTransportDiagnostic};
use futures::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

/// Spawn a tiny HTTP/1.1 server that, for any request, writes a 200 SSE
/// response (with the right headers), then parks the connection without
/// sending any body bytes. Returns the address and a `Cancel` handle the
/// test can use to terminate the server when done.
async fn spawn_held_sse_server() -> (std::net::SocketAddr, Arc<AtomicBool>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw listener");
    let addr = listener.local_addr().expect("local_addr");
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_clone = cancel.clone();

    tokio::spawn(async move {
        loop {
            if cancel_clone.load(Ordering::Relaxed) {
                break;
            }
            let accept = tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
            let Ok(Ok((mut stream, _peer))) = accept else {
                continue;
            };

            // We deliberately do NOT read the request body — we just write
            // the response headers and hold the connection open until the
            // test cancels us. The client's `next_line()` will wait for the
            // first body chunk, the TTFT guard will fire, and the test
            // will complete.
            let response = "HTTP/1.1 200 OK\r\n\
                           content-type: text/event-stream\r\n\
                           cache-control: no-cache\r\n\
                           connection: close\r\n\
                           transfer-encoding: chunked\r\n\
                           \r\n";
            if stream.write_all(response.as_bytes()).await.is_err() {
                continue;
            }
            let _ = stream.flush().await;
            // Park this connection until the test cancels. We never write
            // any chunked-body bytes, so the client side never sees a
            // `data:` event.
            while !cancel_clone.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let _ = stream.shutdown().await;
        }
    });

    (addr, cancel)
}

/// Drain the stream until it ends. Returns the first error yielded, if any.
async fn drain_for_error(
    stream: &mut std::pin::Pin<Box<dyn futures::Stream<Item = anyhow::Result<String>> + Send>>,
) -> Option<anyhow::Error> {
    let mut error = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(_) => continue,
            Err(e) => {
                error = Some(e);
                break;
            }
        }
    }
    error
}

#[tokio::test]
async fn stream_sse_first_event_timeout_emits_typed_transport_error() {
    // AC1: a successful 200 SSE response that never delivers a usable
    // `data:` event must surface as a typed exhausted-transport error (the
    // retryable provider-failure class), not an empty turn.
    let (addr, cancel) = spawn_held_sse_server().await;

    // Use a 200 ms override so the test fires in well under a second.
    // Production callers use `ApiClient::new()` (no override); this override
    // is gated behind a `#[doc(hidden)]` test-only constructor so it does
    // not leak into production call sites.
    let client = ApiClient::new_with_first_event_budget_override(Duration::from_millis(200));
    let mut stream = client.stream_sse(
        &format!("http://{addr}/sse"),
        json!({"model": "gpt-4o", "messages": [{"role": "user", "content": "hi"}]}),
        &AuthMethod::NoAuth,
        HeaderMap::new(),
    );

    let err = drain_for_error(&mut stream)
        .await
        .expect("stream should yield a typed error from the TTFT guard");

    // The diagnostic carries the exact UTF-8 length of the serialized body.
    let provider_err = err
        .downcast_ref::<ProviderError>()
        .unwrap_or_else(|| panic!("expected ProviderError, got: {err:?}"));
    assert_eq!(
        provider_err,
        &ProviderError::ExhaustedTransport(ExhaustedTransportDiagnostic {
            category: ExhaustedTransportCategory::SseFirstEventTimeout,
            estimated_payload_chars: 62,
        }),
        "expected typed exhausted transport diagnostic, got: {provider_err:?}"
    );

    // The error message must mention the TTFT context so operators can
    // distinguish it from a generic stream-timeout. The exact wording is
    // implementation-defined but the substring "first-event" + "TTFT" is
    // stable.
    let display = format!("{err}");
    assert!(
        display.contains("first-event") || display.contains("TTFT"),
        "error message should reference first-event/TTFT: {display}"
    );

    cancel.store(true, Ordering::Relaxed);
}

#[tokio::test]
async fn stream_sse_first_event_timeout_uses_override_for_reasoning_floor() {
    // A reasoning model (`o1`) normally derives a 600 s first-event budget
    // from the floor. The override must CAP that floor — we use a 100 ms
    // override and confirm the guard still fires within the override window.
    // This pins the precedence: explicit override > derived budget (the
    // first_event_budget logic already enforces explicit > floor > default,
    // and this test confirms the override path the transport exposes is
    // honored the same way in `stream_sse`).
    let (addr, cancel) = spawn_held_sse_server().await;

    let client = ApiClient::new_with_first_event_budget_override(Duration::from_millis(100));
    let mut stream = client.stream_sse(
        &format!("http://{addr}/sse"),
        json!({"model": "o1", "messages": []}),
        &AuthMethod::NoAuth,
        HeaderMap::new(),
    );

    let err = drain_for_error(&mut stream)
        .await
        .expect("stream should yield a typed error from the TTFT guard");

    assert_eq!(
        err.downcast_ref::<ProviderError>(),
        Some(&ProviderError::ExhaustedTransport(
            ExhaustedTransportDiagnostic {
                category: ExhaustedTransportCategory::SseFirstEventTimeout,
                estimated_payload_chars: 28,
            }
        )),
        "reasoning model + override must produce a typed timeout diagnostic: {err:?}"
    );

    cancel.store(true, Ordering::Relaxed);
}

#[tokio::test]
async fn stream_sse_keeps_streaming_after_first_event_within_budget() {
    // Sanity check (counter-test): a first event delivered before the
    // override budget elapses must NOT trigger the guard. We emit a
    // single `data:` event then close the stream — no error expected.
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/sse"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n"),
        )
        .mount(&server)
        .await;

    // Override is large enough that the first event should land well
    // inside the budget.
    let client = ApiClient::new_with_first_event_budget_override(Duration::from_secs(5));
    let mut stream = client.stream_sse(
        &format!("{}/sse", server.uri()),
        json!({"model": "gpt-4o", "messages": []}),
        &AuthMethod::NoAuth,
        HeaderMap::new(),
    );

    let mut count = 0usize;
    while let Some(item) = stream.next().await {
        item.unwrap_or_else(|e| panic!("unexpected error: {e:?}"));
        count += 1;
    }
    assert_eq!(count, 1, "exactly one data event expected, got {count}");
}
