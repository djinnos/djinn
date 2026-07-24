//! Agent-facing `run_verification` handler (epic hexh).
//!
//! This is a pure CLIENT of the one authoritative final-verification
//! coordinator. It never opens a second attempt, never writes a passing row,
//! and never clones recording logic — it routes the request through
//! [`djinn_slot::final_verification::coordinate_final_verification_for_agent`],
//! the same consult-or-run entry the completion-intent (`submit_work` /
//! `submit_review`) path uses.
//!
//! The handler owns three things the coordinator does not:
//!   * per-session rate limiting, enforced BEFORE lease acquisition;
//!   * resolving the session's active task run; and
//!   * rendering the typed outcome into a bounded JSON tool result.
//!
//! Telemetry (exactly one bounded tool-attempt outcome, plus full/subset
//! selection and per-check counters) is emitted inside the coordinator client.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::UNIX_EPOCH;

use tokio_util::sync::CancellationToken;

use djinn_slot::final_verification::{
    AgentRunVerificationOutcome, FinalVerificationCoordinatorRequest,
    FinalVerificationPreLeaseGate, FinalVerificationRateLimited, FinalVerificationRunPermit,
    coordinate_final_verification_for_agent,
};
use djinn_slot::host::SlotContext;

const HOUR_SECS: u64 = 3600;

/// Per-session limits for `run_verification`, configured via the established
/// environment-variable pattern with safe defaults (one concurrent invocation
/// plus a small hourly budget).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RunVerificationLimits {
    pub max_concurrent: usize,
    pub hourly_budget: usize,
}

impl RunVerificationLimits {
    /// Env: `DJINN_RUN_VERIFICATION_MAX_CONCURRENT` (default `1`, floored at 1)
    /// and `DJINN_RUN_VERIFICATION_HOURLY_BUDGET` (default `8`, floored at 1).
    pub fn from_env() -> Self {
        let max_concurrent = std::env::var("DJINN_RUN_VERIFICATION_MAX_CONCURRENT")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(1)
            .max(1);
        let hourly_budget = std::env::var("DJINN_RUN_VERIFICATION_HOURLY_BUDGET")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(8)
            .max(1);
        Self {
            max_concurrent,
            hourly_budget,
        }
    }
}

impl Default for RunVerificationLimits {
    fn default() -> Self {
        Self {
            max_concurrent: 1,
            hourly_budget: 8,
        }
    }
}

#[derive(Default)]
struct LimiterState {
    /// Live count of in-flight runs holding a permit.
    active: usize,
    /// Unix-second timestamps of run starts within the trailing hour.
    recent_starts: Vec<u64>,
}

/// Per-session rate limiter shared across the turns of one session. Concurrency
/// is a live gauge; the hourly budget is a sliding window of run starts.
pub(crate) struct SessionVerificationRateLimiter {
    limits: RunVerificationLimits,
    state: Arc<Mutex<LimiterState>>,
}

impl SessionVerificationRateLimiter {
    pub(crate) fn new(limits: RunVerificationLimits) -> Self {
        Self {
            limits,
            state: Arc::new(Mutex::new(LimiterState::default())),
        }
    }

    fn gate(&self, now_unix_secs: u64) -> RateLimitGate {
        RateLimitGate {
            limits: self.limits,
            state: Arc::clone(&self.state),
            now_unix_secs,
        }
    }
}

/// One-shot pre-lease gate handed to the coordinator client. `acquire` is
/// consulted exactly once, after a consult miss and before lease acquisition.
struct RateLimitGate {
    limits: RunVerificationLimits,
    state: Arc<Mutex<LimiterState>>,
    now_unix_secs: u64,
}

/// RAII concurrency permit: decrements the live count when the run returns.
struct ConcurrencyPermit {
    state: Arc<Mutex<LimiterState>>,
}

impl Drop for ConcurrencyPermit {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.active = state.active.saturating_sub(1);
    }
}

impl FinalVerificationPreLeaseGate for RateLimitGate {
    fn acquire(&mut self) -> Result<FinalVerificationRunPermit, FinalVerificationRateLimited> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let cutoff = self.now_unix_secs.saturating_sub(HOUR_SECS);
        state.recent_starts.retain(|&start| start >= cutoff);
        // Hourly budget is checked first: an exhausted budget is the most
        // durable denial and never consumes a concurrency slot.
        if state.recent_starts.len() >= self.limits.hourly_budget {
            let retry_after_seconds = state
                .recent_starts
                .iter()
                .min()
                .map(|&oldest| (oldest + HOUR_SECS).saturating_sub(self.now_unix_secs));
            return Err(FinalVerificationRateLimited {
                scope: "hourly".to_owned(),
                detail: format!(
                    "hourly run_verification budget of {} reached for this session",
                    self.limits.hourly_budget
                ),
                retry_after_seconds,
            });
        }
        if state.active >= self.limits.max_concurrent {
            return Err(FinalVerificationRateLimited {
                scope: "concurrent".to_owned(),
                detail: format!(
                    "{} concurrent run_verification invocation(s) already in flight for this session",
                    self.limits.max_concurrent
                ),
                retry_after_seconds: None,
            });
        }
        state.active += 1;
        state.recent_starts.push(self.now_unix_secs);
        Ok(FinalVerificationRunPermit::new(Box::new(
            ConcurrencyPermit {
                state: Arc::clone(&self.state),
            },
        )))
    }
}

fn now_unix_secs(ctx: &SlotContext) -> u64 {
    ctx.clock
        .now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

async fn resolve_active_task_run(ctx: &SlotContext, task_id: &str) -> Result<String, String> {
    djinn_db::repositories::task_run::TaskRunRepository::new(ctx.db.clone())
        .list_for_task(task_id)
        .await
        .map_err(|error| format!("could not resolve task run: {error}"))?
        .into_iter()
        .find(|run| matches!(run.status.as_str(), "starting" | "running"))
        .map(|run| run.id)
        .ok_or_else(|| "no active task run is available for verification".to_owned())
}

/// Render the typed coordinator outcome into a bounded JSON tool result. Full
/// command output is never included (it stays in structured audit/log storage);
/// only bounded per-check facts and a persisted run id are surfaced.
fn render_outcome(outcome: &AgentRunVerificationOutcome) -> serde_json::Value {
    match outcome {
        AgentRunVerificationOutcome::Hit { evidence, checks } => serde_json::json!({
            "outcome": "hit",
            "reused": true,
            "run_id": evidence.persisted_run_id,
            "completed_at": evidence.completed_at,
            "manifest_version": evidence.manifest_version,
            "required_checks": evidence.required_checks,
            "checks": checks,
            "message": "Verification reused an identical prior passing run; no commands were executed.",
        }),
        AgentRunVerificationOutcome::RanPass { evidence, checks } => serde_json::json!({
            "outcome": "ran-pass",
            "reused": false,
            "run_id": evidence.persisted_run_id,
            "completed_at": evidence.completed_at,
            "manifest_version": evidence.manifest_version,
            "required_checks": evidence.required_checks,
            "checks": checks,
            "message": "Verification ran and every required check passed.",
        }),
        AgentRunVerificationOutcome::RanFail { checks, reason } => serde_json::json!({
            "outcome": "ran-fail",
            "reused": false,
            "checks": checks,
            "reason": reason,
            "message": "Verification ran and at least one required check failed; no passing record was written. Fix the failures and re-run.",
        }),
        AgentRunVerificationOutcome::Error { detail } => serde_json::json!({
            "outcome": "error",
            "detail": detail,
            "message": "Verification could not complete due to an infrastructure error.",
        }),
        AgentRunVerificationOutcome::RateLimited {
            scope,
            detail,
            retry_after_seconds,
        } => serde_json::json!({
            "outcome": "rate-limited",
            "scope": scope,
            "detail": detail,
            "retry_after_seconds": retry_after_seconds,
            "message": "run_verification is rate limited for this session; no verification was started and no lease was acquired.",
        }),
        AgentRunVerificationOutcome::NotConfigured => serde_json::json!({
            "outcome": "not-configured",
            "message": "This project declares no final-verification plan; there is nothing to verify.",
        }),
    }
}

/// Handle one `run_verification` tool call. Resolves the active task run,
/// applies per-session rate limiting via the supplied limiter, and routes the
/// consult-or-run through the authoritative coordinator client.
pub(crate) async fn run_verification(
    limiter: &SessionVerificationRateLimiter,
    task_id: &str,
    role_name: &str,
    cancellation: CancellationToken,
    ctx: &SlotContext,
) -> serde_json::Value {
    let task_run_id = match resolve_active_task_run(ctx, task_id).await {
        Ok(task_run_id) => task_run_id,
        Err(detail) => {
            let outcome = AgentRunVerificationOutcome::Error { detail };
            return render_outcome(&outcome);
        }
    };
    tracing::debug!(
        task_id = %task_id,
        task_run_id = %task_run_id,
        role = %role_name,
        "run_verification tool call routed to the final-verification coordinator client"
    );
    let mut gate = limiter.gate(now_unix_secs(ctx));
    let request = FinalVerificationCoordinatorRequest {
        task_id: task_id.to_owned(),
        task_run_id,
        cancellation,
    };
    let outcome = coordinate_final_verification_for_agent(request, ctx, &mut gate).await;
    render_outcome(&outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(max_concurrent: usize, hourly_budget: usize) -> SessionVerificationRateLimiter {
        SessionVerificationRateLimiter::new(RunVerificationLimits {
            max_concurrent,
            hourly_budget,
        })
    }

    #[test]
    fn hourly_budget_denies_after_exhaustion_without_a_permit() {
        let limiter = limiter(4, 2);
        // Two runs consume the hourly budget; both start and immediately drop
        // their permits so concurrency is never the limiting factor.
        for _ in 0..2 {
            let mut gate = limiter.gate(1_000);
            let permit = gate.acquire().expect("within budget");
            drop(permit);
        }
        let mut gate = limiter.gate(1_000);
        let denied = gate.acquire().expect_err("hourly budget exhausted");
        assert_eq!(denied.scope, "hourly");
        assert_eq!(denied.retry_after_seconds, Some(HOUR_SECS));
    }

    #[test]
    fn hourly_window_slides_so_old_starts_do_not_count() {
        let limiter = limiter(4, 1);
        {
            let mut gate = limiter.gate(1_000);
            drop(gate.acquire().expect("first run within budget"));
        }
        // Same second: budget of 1 is spent.
        let mut gate_now = limiter.gate(1_000);
        assert_eq!(
            gate_now.acquire().expect_err("budget spent").scope,
            "hourly"
        );
        // More than an hour later the old start ages out of the window.
        let mut gate_later = limiter.gate(1_000 + HOUR_SECS + 1);
        assert!(gate_later.acquire().is_ok());
    }

    #[test]
    fn concurrency_limit_denies_while_a_permit_is_held_and_recovers_on_drop() {
        let limiter = limiter(1, 100);
        let mut gate = limiter.gate(2_000);
        let held = gate.acquire().expect("first concurrent slot");
        let mut gate2 = limiter.gate(2_000);
        let denied = gate2.acquire().expect_err("concurrency exhausted");
        assert_eq!(denied.scope, "concurrent");
        assert_eq!(denied.retry_after_seconds, None);
        drop(held);
        // The slot is released; a subsequent acquire succeeds (still within the
        // hourly budget).
        let mut gate3 = limiter.gate(2_000);
        assert!(gate3.acquire().is_ok());
    }

    #[test]
    fn defaults_are_one_concurrent_and_a_small_hourly_budget() {
        let defaults = RunVerificationLimits::default();
        assert_eq!(defaults.max_concurrent, 1);
        assert_eq!(defaults.hourly_budget, 8);
    }
}
