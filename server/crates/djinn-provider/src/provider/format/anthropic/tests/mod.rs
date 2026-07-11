//! Test module shim for `djinn-provider::format::anthropic`.
//!
//! Originally a single `tests.rs` file. The first wave of epic `456f` split
//! the production `anthropic.rs` into the `anthropic/` directory layout. This
//! `tests.rs` survived intact through that wave, but the file grew to
//! 1,465 lines / 53,825 bytes — under the 1,500-line cap but 2,625 bytes
//! **over** the 50 KB cap enforced by `scripts/check-file-size.sh`. This
//! second wave of `456f` splits the test module into the `tests/`
//! subdirectory layout:
//!
//! - `streaming` — SSE/event parser + tokio stream integration tests (the 6
//!   message-level parser tests, the 2 tokio stream tests, the 2
//!   streaming-SSE-adjacent tests, and the 3 `test_build_request_*` /
//!   `test_system_blocks_*` tests that live above the L437 "Empty-segment
//!   handling" section break).
//! - `replay_roundtrip` — parse → shared serde → Anthropic request
//!   round-trip regressions for signed/redacted thinking, opaque unknown blocks,
//!   tool-use ordering, and the empty-text fallback regression guard.
//! - `request` — system-blocks, build_request, reasoning-effort tests (the
//!   `test_build_request_*` / `test_system_blocks_*` /
//!   `test_serialize_system_blocks_*` / `test_reasoning_effort_*` family below
//!   L437).
//! - `e2e_request` — end-to-end prompt assembly + cache-control cap tests (the
//!   4 `e2e_*` tests + 2 `test_cache_control_*` tests). Owns the
//!   `build_system_message_for_test` local helper.
//! - `cache` — stable-prefix-hash + default-cache-policy + effective_url + RMCP
//!   tests (7 `test_stable_prefix_hash_*` + 3 `test_default_cache_policy_*` /
//!   `test_explicit_metadata_overrides_default_policy` + 1 `test_effective_url_*`
//!   + 2 `test_rmcp_tools_*` / `test_tool_without_schema_*`). Owns the
//!     `drift_guard_fixture` local helper.
//!
//! The 3 shared helpers (`spawn_sse_server`, `test_anthropic_config`,
//! `test_provider`) are defined here and marked `pub(super)` so the 5 sibling
//! test files can reach them via `use super::spawn_sse_server;` etc.

#![allow(clippy::doc_overindented_list_items)]

#[allow(unused_imports)]
pub use super::*;
use crate::provider::{AuthMethod, FormatFamily, ProviderCapabilities, ProviderConfig};
use axum::{Router, routing::post};

mod cache;
mod e2e_request;
mod replay_roundtrip;
mod request;
mod streaming;

/// Spawn a minimal axum server that responds to `POST /v1/messages` with the
/// supplied SSE `body` and HTTP `status`. Returns the base URL the test
/// should point its `ProviderConfig::base_url` at.
pub(super) fn spawn_sse_server(status: u16, body: &'static str) -> String {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .expect("bind local tcp listener");
    let addr = listener.local_addr().expect("local addr");
    listener.set_nonblocking(true).expect("set nonblocking");

    let rt = tokio::runtime::Handle::current();
    rt.spawn(async move {
        let app = Router::new().route(
            "/v1/messages",
            post(move |_req: axum::extract::Request| async move {
                (
                    axum::http::StatusCode::from_u16(status).expect("status"),
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }),
        );

        let tokio_listener =
            tokio::net::TcpListener::from_std(listener).expect("convert to tokio listener");
        axum::serve(tokio_listener, app).await.ok();
    });

    format!("http://{}:{}", addr.ip(), addr.port())
}

/// Build a `ProviderConfig` pinned to the Anthropic wire format and the
/// `claude-3-5-sonnet` model. Used by every test that needs a
/// `AnthropicProvider`.
pub(super) fn test_anthropic_config() -> ProviderConfig {
    ProviderConfig {
        base_url: "https://example.com".to_string(),
        auth: AuthMethod::NoAuth,
        format_family: FormatFamily::Anthropic,
        model_id: "claude-3-5-sonnet".to_string(),
        context_window: 200_000,
        telemetry: None,
        session_affinity_key: None,
        provider_headers: std::collections::HashMap::new(),
        capabilities: ProviderCapabilities {
            streaming: true,
            max_tokens_default: Some(64_000),
        },
        reasoning_effort: None,
        tool_schema_compat: None,
    }
}

/// Build an `AnthropicProvider` wired to the `test_anthropic_config()`.
pub(super) fn test_provider() -> AnthropicProvider {
    AnthropicProvider::new(test_anthropic_config())
}
