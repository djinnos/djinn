//! Deterministic transport diagnostics used by oversized-request recovery.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExhaustedTransportCategory {
    ConnectionReset,
    UnexpectedEof,
    RequestBodyWrite,
    ResponseHeaderTimeout,
    SseFirstEventTimeout,
}

/// `estimated_payload_chars` is UTF-8 bytes; the compatibility name is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExhaustedTransportDiagnostic {
    pub category: ExhaustedTransportCategory,
    pub estimated_payload_chars: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportClassificationInput {
    Eligible(ExhaustedTransportCategory),
    ProviderBody,
    Authentication,
    RateLimit,
    Cancellation,
    OrdinaryServerError,
    PostFirstEvent,
    UnknownTransport,
}

/// Only classifies after ordinary initial-request retries. A pre-body eligible
/// observation wins over later wrapper/context classification.
pub fn classify_exhausted_transport(
    retries_exhausted: bool,
    observed: Option<ExhaustedTransportCategory>,
    input: TransportClassificationInput,
    estimated_payload_chars: usize,
) -> Option<ExhaustedTransportDiagnostic> {
    if !retries_exhausted {
        return None;
    }
    let category = observed.or(match input {
        TransportClassificationInput::Eligible(category) => Some(category),
        TransportClassificationInput::ProviderBody
        | TransportClassificationInput::Authentication
        | TransportClassificationInput::RateLimit
        | TransportClassificationInput::Cancellation
        | TransportClassificationInput::OrdinaryServerError
        | TransportClassificationInput::PostFirstEvent
        | TransportClassificationInput::UnknownTransport => None,
    })?;
    Some(ExhaustedTransportDiagnostic {
        category,
        estimated_payload_chars,
    })
}

pub fn oversized_transport_request(
    estimated_payload_chars: usize,
    known_context_window_tokens: u32,
) -> bool {
    known_context_window_tokens > 0
        && (estimated_payload_chars as u64)
            >= u64::from(known_context_window_tokens.saturating_mul(4))
}

pub fn initial_request_transport_category(
    error: &reqwest::Error,
) -> Option<ExhaustedTransportCategory> {
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("connection reset") || message.contains("reset by peer") {
        Some(ExhaustedTransportCategory::ConnectionReset)
    } else if message.contains("unexpected eof") || message.contains("connection closed") {
        Some(ExhaustedTransportCategory::UnexpectedEof)
    } else if error.is_timeout() {
        Some(ExhaustedTransportCategory::ResponseHeaderTimeout)
    } else if error.is_request()
        && (message.contains("body")
            || message.contains("write")
            || message.contains("send request"))
    {
        Some(ExhaustedTransportCategory::RequestBodyWrite)
    } else {
        None
    }
}
