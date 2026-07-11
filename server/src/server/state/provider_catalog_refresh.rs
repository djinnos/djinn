//! Provider-catalog refresh cadence owned by one task for the lifetime of a server.
//!
//! The loop deliberately has two phases, rather than spawning a short-lived boot
//! retry task and a separate periodic task. This prevents concurrent refreshes of
//! one [`CatalogService`] and means a transient boot outage cannot leave a pod
//! permanently serving its embedded snapshot.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use djinn_provider::catalog::{CatalogService, RefreshStatus};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

pub(crate) const PROVIDER_CATALOG_REFRESH_INTERVAL_ENV: &str =
    "DJINN_PROVIDER_CATALOG_REFRESH_INTERVAL_SECS";
pub(crate) const DEFAULT_PROVIDER_CATALOG_REFRESH_INTERVAL_SECS: u64 = 3_600;
pub(crate) const MIN_PROVIDER_CATALOG_REFRESH_INTERVAL_SECS: u64 = 60;
pub(crate) const MAX_PROVIDER_CATALOG_REFRESH_INTERVAL_SECS: u64 = 86_400;
const INITIAL_BOOT_RETRY_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BOOT_RETRY_BACKOFF: Duration = Duration::from_secs(600);
/// The single task state: boot failures retry until the first successful live
/// fetch, then the same task owns periodic cadence forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefreshLoopPhase {
    BootRetry,
    Periodic,
}

fn next_refresh_phase(status: RefreshStatus) -> RefreshLoopPhase {
    if status == RefreshStatus::Success {
        RefreshLoopPhase::Periodic
    } else {
        RefreshLoopPhase::BootRetry
    }
}

/// Parse the server's provider-catalog refresh configuration without reading
/// process state, allowing the interval contract to be tested deterministically.
pub(crate) fn parse_refresh_interval_secs(value: Option<&str>) -> Result<u64, &'static str> {
    let Some(value) = value else {
        return Ok(DEFAULT_PROVIDER_CATALOG_REFRESH_INTERVAL_SECS);
    };
    let seconds = value
        .parse::<u64>()
        .map_err(|_| "must be a positive integer number of seconds")?;
    if !(MIN_PROVIDER_CATALOG_REFRESH_INTERVAL_SECS..=MAX_PROVIDER_CATALOG_REFRESH_INTERVAL_SECS)
        .contains(&seconds)
    {
        return Err("must be between 60 and 86400 seconds");
    }
    Ok(seconds)
}

pub(crate) fn refresh_interval_from_env() -> Duration {
    match std::env::var(PROVIDER_CATALOG_REFRESH_INTERVAL_ENV) {
        Ok(value) => match parse_refresh_interval_secs(Some(&value)) {
            Ok(seconds) => Duration::from_secs(seconds),
            Err(reason) => {
                tracing::warn!(
                    env = PROVIDER_CATALOG_REFRESH_INTERVAL_ENV,
                    value = %value,
                    %reason,
                    default_seconds = DEFAULT_PROVIDER_CATALOG_REFRESH_INTERVAL_SECS,
                    "invalid provider catalog refresh interval; using default"
                );
                Duration::from_secs(DEFAULT_PROVIDER_CATALOG_REFRESH_INTERVAL_SECS)
            }
        },
        Err(_) => Duration::from_secs(DEFAULT_PROVIDER_CATALOG_REFRESH_INTERVAL_SECS),
    }
}

/// Backoff after `failures` unsuccessful boot attempts. The first failure waits
/// five seconds, then doubles each time through the 600-second cap.
pub(crate) fn boot_retry_backoff(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(7);
    INITIAL_BOOT_RETRY_BACKOFF
        .checked_mul(1_u32 << exponent)
        .unwrap_or(MAX_BOOT_RETRY_BACKOFF)
        .min(MAX_BOOT_RETRY_BACKOFF)
}

/// Upper bound for the one-time startup jitter before periodic cadence begins.
pub(crate) fn startup_jitter_max(interval: Duration) -> Duration {
    Duration::from_secs((interval.as_secs() / 10).min(300))
}

fn startup_jitter(interval: Duration) -> Duration {
    let max = startup_jitter_max(interval).as_secs();
    if max == 0 {
        return Duration::ZERO;
    }
    // A per-process wall-clock sample is sufficient to spread pods which all
    // finish boot around the same time; it is intentionally sampled once only.
    let entropy = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    Duration::from_secs(entropy % (max + 1))
}

fn catalog_counts(catalog: &CatalogService) -> (usize, usize) {
    let providers = catalog.list_providers();
    let models = providers
        .iter()
        .map(|provider| catalog.list_models(&provider.id).len())
        .sum();
    (providers.len(), models)
}

fn log_refresh_outcome(catalog: &CatalogService, interval: Duration, phase: &'static str) {
    let status = catalog.last_refresh_status();
    let source_tier = catalog.source_tier(interval.saturating_mul(2));
    let (providers, models) = catalog_counts(catalog);
    match status {
        RefreshStatus::Success => tracing::info!(
            phase,
            status = ?status,
            source_tier = ?source_tier,
            providers,
            models,
            "provider catalog refresh loop succeeded"
        ),
        RefreshStatus::Never | RefreshStatus::Error => tracing::warn!(
            phase,
            status = ?status,
            source_tier = ?source_tier,
            providers,
            models,
            error = ?catalog.last_refresh_error(),
            "provider catalog refresh loop failed; retaining current catalog"
        ),
    }
}

/// Own both boot retry and steady catalog refresh for a single catalog instance.
/// No other refresh task is created when the first live fetch succeeds.
pub(crate) async fn run_provider_catalog_refresh_loop(
    catalog: CatalogService,
    interval: Duration,
    cancel: CancellationToken,
) {
    let mut failures = 0;
    loop {
        catalog.refresh().await;
        log_refresh_outcome(&catalog, interval, "boot");
        if next_refresh_phase(catalog.last_refresh_status()) == RefreshLoopPhase::Periodic {
            break;
        }

        failures += 1;
        let backoff = boot_retry_backoff(failures);
        tracing::warn!(
            backoff_seconds = backoff.as_secs(),
            failures,
            "provider catalog boot refresh will retry"
        );
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(backoff) => {}
        }
    }

    let jitter = startup_jitter(interval);
    if !jitter.is_zero() {
        tracing::info!(
            jitter_seconds = jitter.as_secs(),
            "provider catalog applying one-time periodic startup jitter"
        );
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(jitter) => {}
        }
    }

    let mut ticker = tokio::time::interval(interval);
    // Skip missed ticks so a slow fetch does not cause a burst of catch-up
    // refreshes; the next attempt remains on the configured steady cadence.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticker.tick() => {
                catalog.refresh().await;
                log_refresh_outcome(&catalog, interval, "periodic");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_loop_transitions_to_periodic_only_after_live_success() {
        assert_eq!(
            next_refresh_phase(RefreshStatus::Never),
            RefreshLoopPhase::BootRetry
        );
        assert_eq!(
            next_refresh_phase(RefreshStatus::Error),
            RefreshLoopPhase::BootRetry
        );
        assert_eq!(
            next_refresh_phase(RefreshStatus::Success),
            RefreshLoopPhase::Periodic
        );
    }

    #[test]
    fn refresh_interval_contract_accepts_boundaries_and_rejects_invalid_values() {
        assert_eq!(parse_refresh_interval_secs(None), Ok(3_600));
        assert_eq!(parse_refresh_interval_secs(Some("60")), Ok(60));
        assert_eq!(parse_refresh_interval_secs(Some("86400")), Ok(86_400));
        assert_eq!(parse_refresh_interval_secs(Some("300")), Ok(300));
        for invalid in ["", "zero", "0", "-1", "59", "86401"] {
            assert!(
                parse_refresh_interval_secs(Some(invalid)).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn boot_backoff_doubles_then_caps() {
        assert_eq!(boot_retry_backoff(1), Duration::from_secs(5));
        assert_eq!(boot_retry_backoff(2), Duration::from_secs(10));
        assert_eq!(boot_retry_backoff(7), Duration::from_secs(320));
        assert_eq!(boot_retry_backoff(8), Duration::from_secs(600));
        assert_eq!(boot_retry_backoff(100), Duration::from_secs(600));
    }

    #[test]
    fn startup_jitter_is_bounded_and_based_on_interval_only_once() {
        assert_eq!(
            startup_jitter_max(Duration::from_secs(60)),
            Duration::from_secs(6)
        );
        assert_eq!(
            startup_jitter_max(Duration::from_secs(3_600)),
            Duration::from_secs(300)
        );
        assert_eq!(
            startup_jitter_max(Duration::from_secs(86_400)),
            Duration::from_secs(300)
        );
    }
}
