//! Private protocol-v1 catalog-service wrapper servers and adapters.
//!
//! This crate deliberately exposes only bounded protocol errors. In particular,
//! backend errors, tenant identifiers, and connection credentials never cross
//! the Unix control socket or enter logs.
//!
//! Each adapter returns a self-describing lease URL under only the
//! catalog-configured environment names. This module holds the shared protocol
//! core (bounded error, idempotency key, validation, and secret generation); the
//! per-service adapters live in [`postgres`] and [`redis`].

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use djinn_sandbox::service_provisioning::{CONTROL_PROTOCOL_REVISION, Request, Response};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::net::UnixListener;
use tokio::sync::Notify;

mod postgres;
mod redis;

pub use postgres::{PostgresAdapter, WrapperServer};
pub use redis::{RedisAdapter, RedisWrapperServer};

mod rabbitmq;
pub use rabbitmq::{RabbitAdapter, RabbitWrapperServer};

const OPERATION_DEADLINE: Duration = Duration::from_secs(15);
const MAX_IDENTIFIER_LEN: usize = 64;
const MAX_LINE_LEN: usize = 4096;
const LEASE_PREFIX: &str = "lease_";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterError {
    Rejected,
}

/// The request identity is the idempotency key for CreateFresh. Keeping it
/// associated with the returned lease preserves delete authority after a
/// response is lost and the caller retries.
#[derive(Clone, Hash, PartialEq, Eq)]
struct CreationKey {
    attempt_id: String,
    lease_nonce: String,
}

#[derive(Clone)]
struct CreatedLease {
    lease_id: String,
    environment: BTreeMap<String, String>,
}

/// Wake every registered request and retain one permit for a request that has
/// observed `Creating` but has not registered its waiter yet. The authoritative
/// `CreationState` re-check in `await_creation` remains the source of truth;
/// the stored permit additionally makes the completion edge itself lossless.
fn signal_completion(notify: &Notify) {
    notify.notify_waiters();
    notify.notify_one();
}

/// Bind a wrapper control socket, replacing any stale socket file, and relax its
/// mode so the uid-10001 worker in the same Pod can connect to a socket the root
/// wrapper process created. The Pod boundary is the security perimeter; the
/// control emptyDir is private to the worker and its wrapper sidecars.
fn bind_control_socket(socket: &Path) -> std::io::Result<UnixListener> {
    if socket.exists() {
        std::fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666))?;
    }
    Ok(listener)
}

fn request_revision(request: &Request) -> u32 {
    match request {
        Request::CreateFresh { revision, .. }
        | Request::Ready { revision, .. }
        | Request::Delete { revision, .. } => *revision,
    }
}

fn error_response(code: &str) -> Response {
    Response::Error {
        revision: CONTROL_PROTOCOL_REVISION,
        code: code.to_owned(),
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn valid_environment_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_LEN
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 0 {
                byte == b'_' || byte.is_ascii_alphabetic()
            } else {
                byte == b'_' || byte.is_ascii_alphanumeric()
            }
        })
}

fn generated(prefix: &str, bytes: usize) -> Result<String, AdapterError> {
    let mut random = vec![0; bytes];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| AdapterError::Rejected)?;
    Ok(format!("{prefix}{}", hex::encode(random)))
}

fn random_password() -> Result<String, AdapterError> {
    let mut random = [0; 32];
    SystemRandom::new()
        .fill(&mut random)
        .map_err(|_| AdapterError::Rejected)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifier_validation_is_bounded_and_strict() {
        assert!(valid_identifier("attempt_1-A"));
        assert!(!valid_identifier(""));
        assert!(!valid_identifier("has space"));
        assert!(!valid_identifier(&"a".repeat(MAX_IDENTIFIER_LEN + 1)));
    }

    #[tokio::test]
    async fn completion_signal_retains_a_permit_for_a_late_waiter() {
        let notify = Notify::new();

        // Exercise the part of the handshake that `notify_waiters()` alone
        // cannot provide: completion happens with no registered waiter.
        signal_completion(&notify);

        tokio::time::timeout(Duration::from_millis(50), notify.notified())
            .await
            .expect("completion permit was lost before waiter registration");
    }
}
