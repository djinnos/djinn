//! Periodic Codex OAuth keep-alive loop.
//!
//! ChatGPT/Codex OAuth uses single-use, rotating refresh tokens on a sliding
//! window: refresh keeps the chain alive, disuse lets it lapse. Djinn only
//! refreshed lazily (at dispatch / after a 401), so a *connected but idle*
//! Codex plan silently died once OpenAI expired its unused refresh token. This
//! leader-only loop periodically refreshes idle Codex credentials to keep the
//! rotation chain warm, and proactively marks genuinely-dead ones revoked so
//! the owner is prompted to reconnect right away.
//!
//! The refresh logic itself lives in
//! [`djinn_provider::oauth::keepalive::run_codex_keepalive_sweep`]; this module
//! is just the leader-gated ticker, mirroring [`crate::git_maintenance`].

use std::time::Duration;

use tokio::time::MissedTickBehavior;

use djinn_provider::repos::CredentialRepository;

use crate::server::AppState;

const DEFAULT_INTERVAL_SECS: u64 = 6 * 60 * 60;
const INTERVAL_ENV: &str = "DJINN_CODEX_KEEPALIVE_INTERVAL_SECS";

/// Spawn the periodic Codex keep-alive task. Leader-only (started from
/// `become_leader`), runs until `state.cancel()` fires.
pub fn spawn(state: AppState) {
    let interval = parse_interval(std::env::var(INTERVAL_ENV).ok().as_deref());
    let cancel = state.cancel().clone();

    tokio::spawn(async move {
        tracing::info!(?interval, "codex_keepalive loop starting");
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Consume the immediate first tick so we don't sweep right at boot,
        // during the leadership transition.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!("codex_keepalive loop cancelled");
                    break;
                }
                _ = ticker.tick() => run_tick(&state).await,
            }
        }
    });
}

async fn run_tick(state: &AppState) {
    let repo = CredentialRepository::new(state.db().clone(), state.event_bus());
    let stats = djinn_provider::oauth::keepalive::run_codex_keepalive_sweep(&repo).await;
    tracing::debug!(?stats, "codex_keepalive: tick complete");
}

fn parse_interval(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_uses_positive_override_else_default() {
        assert_eq!(parse_interval(Some("3600")), Duration::from_secs(3600));
        assert_eq!(
            parse_interval(None),
            Duration::from_secs(DEFAULT_INTERVAL_SECS)
        );
        assert_eq!(
            parse_interval(Some("0")),
            Duration::from_secs(DEFAULT_INTERVAL_SECS)
        );
        assert_eq!(
            parse_interval(Some("not-a-number")),
            Duration::from_secs(DEFAULT_INTERVAL_SECS)
        );
    }
}
