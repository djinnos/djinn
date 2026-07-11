use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use djinn_core::clock::{Clock, SystemClock};
use serde::{Deserialize, Serialize};

/// Number of consecutive failures before circuit breaker trips.
const CIRCUIT_BREAKER_THRESHOLD: u32 = 3;
/// Initial cooldown after first circuit-breaker trip: 5 seconds.
const INITIAL_COOLDOWN: Duration = Duration::from_secs(5);
/// Maximum escalating cooldown: 4 hours.
///
/// This is the ceiling of the *escalating* auto-disable ladder
/// (`compute_cooldown`): each consecutive trip triples the previous cooldown
/// (5s → 15s → 45s … → 4h) so a persistently-broken model stays demoted for
/// progressively longer instead of re-enabling on a fixed short TTL. The old
/// ceiling was 5 minutes, which let a genuinely-broken model
/// (`xiaomi-token-plan-sgp/mimo-v2.5-pro`, 78 failures vs 16 successes) trip the
/// breaker ~50 times in one night: disable → 5-min cooldown → re-enable → grab a
/// priority task → produce nothing for 30 minutes → trip → repeat, burning a
/// worker slot every cycle. A multi-hour ceiling means each successive trip on a
/// truly-dead model costs the fleet exponentially less.
const MAX_COOLDOWN: Duration = Duration::from_secs(4 * 60 * 60);

/// Minimum cooldown applied when a model is tripped via [`HealthTracker::record_stall`]
/// (a zero-token / first-LLM-call-hung stall). This is the key knob that makes
/// the model breaker actually drive *failover*: a stalled task gets re-dispatched
/// after an escalating *task* cooldown that starts at 60s and grows to 120s/240s.
/// If a stall only tripped the ordinary 5s model cooldown the model would be
/// "healthy" again long before the task re-dispatches, so dispatch would re-pick
/// the same bad model — no failover. Holding the model unavailable for 5 minutes
/// guarantees the model is still cooling down on the task's next dispatch (even at
/// the 240s task-cooldown rung), so the next model in the creator's ordered list
/// is selected instead. The cooldown still auto-expires, so a one-off stall
/// self-heals; repeated stalls escalate via the normal ladder and pin at the cap.
///
/// Deliberately a *fixed* 5 minutes rather than `MAX_COOLDOWN`: now that the
/// escalation ceiling is multi-hour, aliasing this to `MAX_COOLDOWN` would floor
/// a *single* one-off stall at 4h and destroy the self-heal property. The stall
/// floor only needs to outlast the 240s task-redispatch ladder; repeated stalls
/// still escalate past this floor via `compute_cooldown` up to `MAX_COOLDOWN`.
const STALL_MIN_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// Rolling-window trip-rate ceiling: if a `(scope, model)` bucket's breaker trips
/// this many times within [`TRIP_RATE_WINDOW`], it is **hard-disabled** — held
/// unavailable with NO auto-expiry until a human re-enables it via the
/// `model_health` `enable` action. This is the backstop the escalating cooldown
/// alone can't provide: even a 4h ceiling still lets a hopeless model flap a
/// couple of times a day, and the incident model was flapping every 10–40 min.
/// A model that keeps tripping despite the escalating cooldown is not "recovering
/// on a clock" — it is broken, and continuing to auto-re-enable it just keeps
/// feeding it priority tasks it cannot complete. `8` trips in `6h` is well above
/// what a transiently-flaky-but-usable model produces, yet well below the ~50
/// trips/night the incident model sustained.
const TRIP_RATE_CEILING: usize = 8;
/// Rolling window over which [`TRIP_RATE_CEILING`] trips force a hard-disable.
const TRIP_RATE_WINDOW: Duration = Duration::from_secs(6 * 60 * 60);

/// Number of *consecutive* throttle trips (`record_stall(escalate=false)`) with
/// no intervening success after which a throttle stops being treated as a
/// clock-resetting quota blip and starts escalating like a genuine failure.
///
/// A throttle-classified stall normally applies only the fixed
/// [`STALL_MIN_COOLDOWN`] floor and does NOT advance `disable_ttl_trips` or the
/// hard-disable trip-rate window, because a quota that resets in minutes is not
/// a broken model. But a token *plan / subscription* that has been over-quota
/// for **days** produces the same 429 every session and flaps forever: disable
/// 5 min → re-enable → grab a slot → crash in <10s → repeat, with a ~30-minute
/// task-redispatch cooldown between attempts (one production task lost 5.75h to
/// 8 consecutive 429 crashes). Because the throttle never escalates, the
/// 8-trips/6h hard-disable backstop ([`TRIP_RATE_CEILING`]) never catches it.
///
/// Once a bucket has throttle-cooldowned this many consecutive times with no
/// success in between, further throttle trips are escalated — they advance
/// `disable_ttl_trips` (growing the cooldown past the 5-minute floor) AND count
/// toward the hard-disable ceiling — so a genuinely-dead subscription is
/// eventually pinned instead of flapping indefinitely. A single successful
/// session fully resets the streak (the plan's quota came back), returning the
/// model to the gentle clock-reset treatment. `6` consecutive trips is well
/// past what a minutes-long quota window produces (which self-heals after one
/// or two cooldowns and then succeeds) yet reached within ~30 minutes of a
/// truly-dead plan flapping.
const PERSISTENT_THROTTLE_TRIP_THRESHOLD: u32 = 6;
/// Wall-clock companion to [`PERSISTENT_THROTTLE_TRIP_THRESHOLD`]: if a bucket
/// has been continuously throttle-cooldowning (no intervening success) for at
/// least this long, escalate even when the consecutive-trip *count* is still
/// low. Sparse-but-persistent throttling — e.g. a provider-stated multi-minute
/// `retry-after` producing few, long cooldowns — is just as dead as rapid
/// flapping, and the wall-clock bound catches it without waiting for the count.
const PERSISTENT_THROTTLE_WINDOW: Duration = Duration::from_secs(60 * 60);

/// Identifies a single circuit-breaker bucket.
///
/// Health is tracked **per `(scope, model_id)`**, not globally per model. The
/// throttling/outage the breaker reacts to is almost always *credential-scoped*
/// — most acutely the ChatGPT Codex OAuth backend, which rate-limits per
/// **account**: it answers an over-quota account with empty `response.completed`
/// turns (HTTP 200, zero tokens) that the coordinator sees as a zero-token
/// stall. A *global* key let one throttled user's stalls disable a model for
/// **every** user's task dispatch (the model would read `auto_disabled` for
/// everyone, even though their own credential is healthy). Keying by the owning
/// user isolates that blast radius.
///
/// `scope` is the owning user id (`tasks.created_by_user_id`); `None` is the
/// shared bucket for system / unowned work that runs on the org-shared
/// credential. An org-shared credential that is genuinely down therefore trips
/// once per distinct user that hits it (rather than once globally) — slightly
/// slower to converge, but each bucket still self-heals on cooldown, and a
/// truly-bad model is demoted independently for everyone who touches it.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize)]
pub struct HealthKey {
    pub scope: Option<String>,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BreakerDebugEntry {
    pub scope: Option<String>,
    pub model: String,
    pub state: String,
    pub until: Option<String>,
    pub consecutive_failures: u32,
}

/// Circuit-breaker state used by metrics snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum BreakerState {
    Closed,
    HalfOpen,
    Open,
}

impl BreakerState {
    /// Numeric Prometheus gauge value for `djinn_breaker_state{scope,model}`.
    ///
    /// `HalfOpen` is emitted as `0.5`: the original metrics proposal only named
    /// the closed/open endpoints (`0`/`1`), and the midpoint preserves that
    /// ordering while making cooldown-expired trial buckets distinguishable.
    pub fn metric_value(self) -> f64 {
        match self {
            Self::Closed => 0.0,
            Self::HalfOpen => 0.5,
            Self::Open => 1.0,
        }
    }
}

/// Owned, non-async breaker snapshot suitable for scrape-time metrics emission.
#[derive(Clone, Debug, PartialEq)]
pub struct BreakerMetricSnapshot {
    /// Metrics label; `None`/shared health buckets are rendered as `shared`.
    pub scope: String,
    pub model: String,
    pub state: BreakerState,
    pub value: f64,
}

impl HealthKey {
    pub fn new(scope: Option<&str>, model_id: &str) -> Self {
        Self {
            scope: scope.map(str::to_owned),
            model_id: model_id.to_owned(),
        }
    }
}

/// Wire-format health state for a single `(scope, model)` bucket.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelHealth {
    pub model_id: String,
    /// Owning user id this bucket is scoped to; `None` = shared/system bucket.
    /// `#[serde(default)]` keeps older persisted snapshots (which had no scope)
    /// loading as the shared bucket.
    #[serde(default)]
    pub scope: Option<String>,
    pub auto_disabled: bool,
    pub consecutive_failures: u32,
    /// Consecutive failures that are eligible to trip the breaker. This is
    /// persisted separately from diagnostic `consecutive_failures` so
    /// fallback-rescued observations cannot later contribute to breaker trips.
    #[serde(default)]
    pub breaker_eligible_consecutive_failures: u32,
    pub total_failures: u32,
    pub total_successes: u32,
    /// Current escalating-cooldown tier: how many times the breaker has tripped
    /// without an intervening success. Drives `compute_cooldown` (5s·3^tier,
    /// capped at `MAX_COOLDOWN`). Reset to 0 by a successful session.
    pub disable_ttl_trips: u32,
    /// Seconds until the cooldown expires; `None` when not currently disabled
    /// **or when hard-disabled** (a hard-disable has no auto-expiry).
    pub cooldown_seconds_remaining: Option<u64>,
    /// Hard-disabled: the trip-rate ceiling ([`TRIP_RATE_CEILING`] trips within
    /// [`TRIP_RATE_WINDOW`]) was hit, so the bucket is held unavailable with NO
    /// auto-expiry until a human re-enables it. `#[serde(default)]` keeps older
    /// persisted snapshots (which had no such field) loading as not-hard-disabled.
    #[serde(default)]
    pub hard_disabled: bool,
    /// Number of breaker trips currently inside the rolling [`TRIP_RATE_WINDOW`].
    /// Surfaced so operators can see how close a bucket is to the hard-disable
    /// ceiling. `#[serde(default)]` for back-compat with pre-ceiling snapshots.
    #[serde(default)]
    pub trips_in_window: u32,
}

#[derive(Default)]
struct ModelState {
    auto_disabled: bool,
    cooldown_until: Option<Instant>,
    consecutive_failures: u32,
    /// Consecutive failures that are eligible to trip the circuit breaker.
    ///
    /// This intentionally differs from `consecutive_failures`: failover
    /// traversal records every per-candidate failure immediately for
    /// diagnostics, but failures rescued by a later fallback candidate must
    /// remain diagnostic-only forever. Only observations from chains that
    /// actually exhaust increment this counter via
    /// `HealthTracker::apply_breaker_check_for`.
    breaker_eligible_consecutive_failures: u32,
    total_failures: u32,
    total_successes: u32,
    disable_ttl_trips: u32,
    /// Hard-disabled by the trip-rate ceiling — no auto-expiry, human re-enable
    /// only. Distinct from `auto_disabled` (which self-heals on cooldown).
    hard_disabled: bool,
    /// Monotonic timestamps of recent breaker trips, pruned to
    /// [`TRIP_RATE_WINDOW`]. When its length reaches [`TRIP_RATE_CEILING`] the
    /// bucket is hard-disabled. Only genuine (escalating) trips are recorded —
    /// account-quota throttles do not count toward the ceiling *unless* they
    /// have persisted long enough to be escalated (see `record_stall`).
    trip_times: Vec<Instant>,
    /// Consecutive throttle trips (`record_stall(escalate=false)`) with no
    /// intervening success. Drives persistent-throttle escalation: once this
    /// reaches [`PERSISTENT_THROTTLE_TRIP_THRESHOLD`] (or the streak has lasted
    /// [`PERSISTENT_THROTTLE_WINDOW`]) further throttle trips escalate like
    /// genuine trips. Reset to 0 by any success. Runtime-only (not persisted):
    /// the escalation it *produces* — `disable_ttl_trips` / `trip_times` — is
    /// what survives a restart; an in-progress streak restarting from zero on a
    /// leader failover is acceptable.
    consecutive_throttle_trips: u32,
    /// Monotonic timestamp of the first throttle trip in the current streak,
    /// for the [`PERSISTENT_THROTTLE_WINDOW`] wall-clock escalation. `None` when
    /// there is no active throttle streak. Runtime-only (Instants don't persist).
    throttle_streak_started_at: Option<Instant>,
    /// Whether the trip that put this bucket into its current cooldown was a
    /// throttle (`escalate=false`). Consulted at dispatch time
    /// (`is_throttle_cooling`) so the candidate order can deprioritize a model
    /// that is currently throttle-cooling in favor of a healthy lane-mate.
    /// Cleared on success.
    last_trip_throttle: bool,
}

impl ModelState {
    fn is_available(&self, now: Instant) -> bool {
        // Hard-disabled buckets never auto-recover — a human must re-enable.
        if self.hard_disabled {
            return false;
        }
        if !self.auto_disabled {
            return true;
        }
        // Cooldown expired → model auto-re-enables on next availability check.
        matches!(self.cooldown_until, Some(until) if now >= until)
    }

    /// Count of breaker trips still inside the rolling [`TRIP_RATE_WINDOW`].
    fn trips_in_window(&self, now: Instant) -> u32 {
        self.trip_times
            .iter()
            .filter(|t| now.duration_since(**t) < TRIP_RATE_WINDOW)
            .count() as u32
    }

    /// Record a genuine breaker trip against the rolling trip-rate window and
    /// hard-disable the bucket if the ceiling is reached. Returns `true` when
    /// this trip crossed the ceiling (for one-shot logging).
    fn register_trip(&mut self, now: Instant) -> bool {
        // Prune trips that have aged out of the window, then record this one.
        self.trip_times
            .retain(|t| now.duration_since(*t) < TRIP_RATE_WINDOW);
        self.trip_times.push(now);
        if !self.hard_disabled && self.trip_times.len() >= TRIP_RATE_CEILING {
            self.hard_disabled = true;
            // No auto-expiry: clear the cooldown deadline so `is_available`
            // relies solely on the `hard_disabled` gate until a human re-enables.
            self.cooldown_until = None;
            return true;
        }
        false
    }

    fn cooldown_seconds_remaining(&self, now: Instant) -> Option<u64> {
        let until = self.cooldown_until?;
        if until > now {
            Some((until - now).as_secs())
        } else {
            None
        }
    }

    fn to_health(&self, key: &HealthKey, now: Instant) -> ModelHealth {
        ModelHealth {
            model_id: key.model_id.clone(),
            scope: key.scope.clone(),
            // Report as disabled when hard-disabled, or when an ordinary cooldown
            // has not yet expired.
            auto_disabled: self.hard_disabled || (self.auto_disabled && !self.is_available(now)),
            consecutive_failures: self.consecutive_failures,
            breaker_eligible_consecutive_failures: self.breaker_eligible_consecutive_failures,
            total_failures: self.total_failures,
            total_successes: self.total_successes,
            disable_ttl_trips: self.disable_ttl_trips,
            cooldown_seconds_remaining: self.cooldown_seconds_remaining(now),
            hard_disabled: self.hard_disabled,
            trips_in_window: self.trips_in_window(now),
        }
    }

    fn compute_cooldown(&self) -> Duration {
        // Exponential backoff: 5s, 15s, 45s, 135s, 300s (capped).
        // disable_ttl_trips counts how many times this model has been disabled.
        let mut ttl = INITIAL_COOLDOWN;
        for _ in 0..self.disable_ttl_trips {
            ttl = (ttl * 3).min(MAX_COOLDOWN);
        }
        ttl
    }
}

/// A per-task record of the most recent typed provider failure on a task-run,
/// stashed by the slot supervisor-runner for the coordinator's redispatch logic
/// to consult.
///
/// This is a **side-channel**, not part of the circuit-breaker: the breaker is
/// keyed per `(scope, model)`, but the coordinator's terminal-failure streak and
/// escalating redispatch cooldown are keyed per *task* and the coordinator never
/// observes the task-run report directly (it only sees the task reappear as
/// dispatch-ready). The runner records this when it processes a
/// `TaskRunOutcome::Failed` carrying a typed provider class; the coordinator
/// reads-and-clears it at the reappearance/streak site (see A3/A6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskFailureSignal {
    /// The failure was a rate-limit / quota throttle (not a structural failure).
    /// A3: a throttle reappearance must NOT advance the terminal streak.
    pub throttle: bool,
    /// Provider-stated reset window (`Retry-After` / rate-limit-reset), if any.
    /// A6: floors the escalating redispatch cooldown so a multi-hour quota
    /// window isn't probed on the fixed ladder. Only meaningful for throttles.
    pub retry_after_ms: Option<u64>,
}

/// Thread-safe in-memory model health tracker with circuit-breaker logic.
///
/// Circuit breaker: after `CIRCUIT_BREAKER_THRESHOLD` consecutive failures the
/// `(scope, model)` bucket is auto-disabled with an exponentially growing
/// cooldown. Buckets auto re-enable once the cooldown expires. See [`HealthKey`]
/// for why health is tracked per-scope rather than globally per model.
///
/// It also carries a small per-task **side-channel** ([`TaskFailureSignal`],
/// distinct from the breaker buckets) so the coordinator's per-task redispatch
/// logic can learn the class + retry-after of a task-run's last provider failure
/// — which it otherwise cannot see, as it never observes the report directly.
#[derive(Clone)]
pub struct HealthTracker {
    inner: Arc<Mutex<HashMap<HealthKey, ModelState>>>,
    /// Side-channel: task_id → most recent provider-failure signal. Written by
    /// the slot supervisor-runner on a typed provider failure; read-and-cleared
    /// by the coordinator at its redispatch streak/cooldown site. Independent of
    /// the `(scope, model)` breaker buckets above.
    task_failures: Arc<Mutex<HashMap<String, TaskFailureSignal>>>,
}

impl HealthTracker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            task_failures: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Record the most recent typed provider failure for a task (side-channel
    /// for the coordinator's redispatch logic; see [`TaskFailureSignal`]). The
    /// latest failure for a task overwrites any prior one. This does NOT touch
    /// the `(scope, model)` circuit-breaker buckets.
    pub fn note_task_provider_failure(&self, task_id: &str, signal: TaskFailureSignal) {
        self.task_failures
            .lock()
            .unwrap()
            .insert(task_id.to_owned(), signal);
    }

    /// Read-and-clear the most recent provider-failure signal for a task. The
    /// coordinator consults this exactly once per reappearance — when a task it
    /// dispatched becomes dispatch-ready again — so a stale signal can't leak
    /// into a later, unrelated redispatch decision. Returns `None` when the
    /// task's last failure was not a typed provider error (or was already
    /// consumed).
    pub fn take_task_provider_failure(&self, task_id: &str) -> Option<TaskFailureSignal> {
        self.task_failures.lock().unwrap().remove(task_id)
    }

    /// Record a failure **observation** — increments the consecutive/total
    /// failure counters for the `(scope, model)` bucket so the candidate's
    /// failure is reflected in health-state diagnostics immediately, but does
    /// NOT trip the circuit breaker.
    ///
    /// Use this during failover-chain traversal so each candidate's failure is
    /// observed immediately (for diagnostics and per-candidate health state),
    /// while breaker demotion/cooldown is deferred until the chain is exhausted.
    /// The caller is responsible for tracking the chain-scoped list of observed
    /// failures and passing them to [`apply_breaker_check_for`] after chain
    /// exhaustion; this method itself is intentionally chain-agnostic so a
    /// successful fallback cannot leak breaker side effects into later,
    /// unrelated chains.
    pub fn record_failure_observation(&self, scope: Option<&str>, model_id: &str) {
        let mut map = self.inner.lock().unwrap();
        let state = map.entry(HealthKey::new(scope, model_id)).or_default();
        state.consecutive_failures += 1;
        state.total_failures += 1;
    }

    /// Evaluate and apply one circuit-breaker-eligible failure for a single
    /// `(scope, model)` key. Called after failover-chain exhaustion for each
    /// candidate that was observed to fail in that exhausted chain.
    ///
    /// The diagnostic counters were already incremented by
    /// [`record_failure_observation`], so this method must NOT consult
    /// `consecutive_failures` for breaker eligibility. Fallback-rescued chains
    /// also increment that diagnostic counter, and allowing it to trip the
    /// breaker later would leak non-terminal observations into an unrelated
    /// exhausted chain. Instead, this advances a separate breaker-eligible
    /// consecutive-failure counter scoped to chain exhaustions only.
    pub fn apply_breaker_check_for(&self, scope: Option<&str>, model_id: &str) {
        let now = SystemClock::new().now_instant();
        let mut map = self.inner.lock().unwrap();
        let key = HealthKey::new(scope, model_id);
        let Some(state) = map.get_mut(&key) else {
            return;
        };

        // If the previous cooldown expired, clear the flag so we can re-trip.
        if state.auto_disabled && state.is_available(now) {
            state.auto_disabled = false;
            state.cooldown_until = None;
        }

        state.breaker_eligible_consecutive_failures = state
            .breaker_eligible_consecutive_failures
            .saturating_add(1);

        if !state.auto_disabled
            && state.breaker_eligible_consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD
        {
            let cooldown = state.compute_cooldown();
            state.auto_disabled = true;
            state.cooldown_until = Some(now + cooldown);
            state.disable_ttl_trips += 1;
            let hard_disabled = state.register_trip(now);
            djinn_telemetry::breaker::increment_trip();
            tracing::warn!(
                model_id = %key.model_id,
                scope = ?key.scope,
                consecutive_failures = state.consecutive_failures,
                breaker_eligible_consecutive_failures = state.breaker_eligible_consecutive_failures,
                cooldown_secs = cooldown.as_secs(),
                disable_ttl_trips = state.disable_ttl_trips,
                trips_in_window = state.trips_in_window(now),
                hard_disabled,
                "model circuit-breaker tripped after failover-chain exhaustion"
            );
            if hard_disabled {
                tracing::error!(
                    model_id = %key.model_id,
                    scope = ?key.scope,
                    trips_in_window = TRIP_RATE_CEILING,
                    window_hours = TRIP_RATE_WINDOW.as_secs() / 3600,
                    "model breaker hit trip-rate ceiling — HARD-DISABLED until a human \
                     re-enables it via model_health(enable)"
                );
            }
        }
    }

    /// Record a successful invocation.  Resets consecutive failure counter;
    /// clears auto-disable state if the cooldown has expired.
    pub fn record_success(&self, scope: Option<&str>, model_id: &str) {
        let now = SystemClock::new().now_instant();
        let mut map = self.inner.lock().unwrap();
        let state = map.entry(HealthKey::new(scope, model_id)).or_default();
        state.consecutive_failures = 0;
        state.breaker_eligible_consecutive_failures = 0;
        state.total_successes += 1;
        // A productive session is proof the model recovered — reset the
        // escalating-cooldown tier and clear the rolling trip window so the next
        // failure starts fresh at the base cooldown and the hard-disable ceiling
        // isn't reached by ancient, since-recovered trips.
        state.disable_ttl_trips = 0;
        state.trip_times.clear();
        // A success proves the plan's quota is back — fully reset the
        // persistent-throttle escalation state so the next throttle starts a
        // fresh streak with the gentle clock-reset treatment, and clear the
        // throttle-cooling deprioritization flag.
        state.consecutive_throttle_trips = 0;
        state.throttle_streak_started_at = None;
        state.last_trip_throttle = false;
        if state.auto_disabled && state.is_available(now) {
            state.auto_disabled = false;
            state.cooldown_until = None;
        }
    }

    /// Record a failed invocation.  Trips the circuit breaker when the
    /// consecutive failure threshold is reached.
    pub fn record_failure(&self, scope: Option<&str>, model_id: &str) {
        let now = SystemClock::new().now_instant();
        let mut map = self.inner.lock().unwrap();
        let key = HealthKey::new(scope, model_id);
        let state = map.entry(key.clone()).or_default();
        state.consecutive_failures += 1;
        state.breaker_eligible_consecutive_failures += 1;
        state.total_failures += 1;

        // If the previous cooldown expired, clear the flag so we can re-trip.
        if state.auto_disabled && state.is_available(now) {
            state.auto_disabled = false;
            state.cooldown_until = None;
        }

        if !state.auto_disabled
            && state.breaker_eligible_consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD
        {
            let cooldown = state.compute_cooldown();
            state.auto_disabled = true;
            state.cooldown_until = Some(now + cooldown);
            state.disable_ttl_trips += 1;
            let hard_disabled = state.register_trip(now);
            djinn_telemetry::breaker::increment_trip();
            tracing::warn!(
                model_id = %key.model_id,
                scope = ?key.scope,
                consecutive_failures = state.consecutive_failures,
                breaker_eligible_consecutive_failures = state.breaker_eligible_consecutive_failures,
                cooldown_secs = cooldown.as_secs(),
                disable_ttl_trips = state.disable_ttl_trips,
                trips_in_window = state.trips_in_window(now),
                hard_disabled,
                "model circuit-breaker tripped"
            );
            if hard_disabled {
                tracing::error!(
                    model_id = %key.model_id,
                    scope = ?key.scope,
                    trips_in_window = TRIP_RATE_CEILING,
                    window_hours = TRIP_RATE_WINDOW.as_secs() / 3600,
                    "model breaker hit trip-rate ceiling — HARD-DISABLED until a human \
                     re-enables it via model_health(enable)"
                );
            }
        }
    }

    /// Record a **stall** — a session whose first LLM call hung (the
    /// coordinator's zero-token / no-activity kill). This is a strong "this
    /// model/backend is bad right now for this account" signal, so unlike
    /// [`record_failure`] it trips the breaker **immediately** (no
    /// consecutive-failure threshold) and with a cooldown floored at
    /// [`STALL_MIN_COOLDOWN`].
    ///
    /// The floor is what makes failover actually happen: the task that owned
    /// the stalled session is re-dispatched after an escalating *task* cooldown
    /// (60s → 120s → 240s). A 5-minute model cooldown guarantees the model is
    /// still unavailable when the task re-dispatches, so dispatch picks the next
    /// model in the creator's ordered list instead of re-selecting the bad one.
    /// The cooldown still auto-expires (self-heal), and repeated stalls escalate
    /// `disable_ttl_trips` so a persistently-bad model stays demoted at the cap.
    ///
    /// `escalate` controls whether this trip advances `disable_ttl_trips` (which
    /// grows the cooldown cap across repeated trips):
    /// - `true` for a genuine `Failure` / `AuthInvalid` / infra-stall signal — a
    ///   persistently-bad model SHOULD stay demoted longer the more it misbehaves.
    /// - `false` for a **throttle-classified** stall (the Codex/OpenAI empty-200
    ///   account-quota signal). A quota resets on a clock, not on model health, so
    ///   escalating the cooldown cap is wrong: the model isn't getting "more
    ///   broken", the account is merely over quota until its window resets. We
    ///   still apply the `STALL_MIN_COOLDOWN` floor so failover still happens —
    ///   we just don't ratchet the cap **until the throttle proves persistent**.
    ///
    /// Persistent-throttle escalation (see [`PERSISTENT_THROTTLE_TRIP_THRESHOLD`]):
    /// a subscription that has been over-quota for *days* keeps 429-ing every
    /// session and, with `escalate=false`, would flap forever without the
    /// hard-disable backstop ever catching it. Once a bucket has throttle-tripped
    /// enough consecutive times (or for long enough on the wall clock) with no
    /// intervening success, this method escalates the throttle *as if* it were a
    /// genuine trip — ratcheting the cap and counting toward the hard-disable
    /// ceiling. A single success fully resets that streak.
    pub fn record_stall(&self, scope: Option<&str>, model_id: &str, escalate: bool) {
        let now = SystemClock::new().now_instant();
        let mut map = self.inner.lock().unwrap();
        let key = HealthKey::new(scope, model_id);
        let state = map.entry(key.clone()).or_default();
        state.consecutive_failures += 1;
        state.total_failures += 1;

        // If the previous cooldown expired, clear the flag so we can re-trip
        // (and so `disable_ttl_trips` keeps escalating across stalls).
        if state.auto_disabled && state.is_available(now) {
            state.auto_disabled = false;
            state.cooldown_until = None;
        }

        // Already cooling down from an earlier trip — don't shorten or reset it.
        if state.auto_disabled {
            return;
        }

        // Persistent-throttle escalation. A throttle (`escalate=false`) normally
        // resets on a clock, not on model health, so it does NOT ratchet the
        // escalating cooldown cap or count toward the hard-disable ceiling — a
        // quota-limited account is not a broken model. But a plan/subscription
        // that has been over-quota for *days* keeps flapping (disable 5min →
        // re-enable → crash in <10s → repeat) and never escalates, so the
        // hard-disable backstop never catches it. Track consecutive throttle
        // trips (no intervening success); once the streak is long enough — by
        // count OR wall-clock — escalate this and every subsequent throttle
        // trip like a genuine failure so the 8-trips/6h ladder can eventually
        // pin a dead subscription. A single success fully resets the streak.
        let effective_escalate = if escalate {
            // A genuine failure/stall trip breaks any throttle streak — it is
            // already escalating on its own ladder.
            state.consecutive_throttle_trips = 0;
            state.throttle_streak_started_at = None;
            true
        } else {
            state.consecutive_throttle_trips = state.consecutive_throttle_trips.saturating_add(1);
            let streak_started_at = *state.throttle_streak_started_at.get_or_insert(now);
            state.consecutive_throttle_trips >= PERSISTENT_THROTTLE_TRIP_THRESHOLD
                || now.duration_since(streak_started_at) >= PERSISTENT_THROTTLE_WINDOW
        };

        // Trip immediately, cooldown floored at STALL_MIN_COOLDOWN so it
        // outlasts the task's redispatch cooldown and forces failover.
        let cooldown = state.compute_cooldown().max(STALL_MIN_COOLDOWN);
        state.auto_disabled = true;
        state.cooldown_until = Some(now + cooldown);
        // Remember whether this cooldown was throttle-induced so dispatch can
        // deprioritize a currently-cooling model in favor of a healthy lane-mate.
        state.last_trip_throttle = !escalate;
        // A non-escalated throttle only floors the cooldown; an escalated trip
        // (genuine failure, OR a throttle that has persisted past the threshold)
        // also ratchets `disable_ttl_trips` and counts toward the hard-disable
        // ceiling.
        let hard_disabled = if effective_escalate {
            state.disable_ttl_trips += 1;
            state.register_trip(now)
        } else {
            false
        };
        djinn_telemetry::breaker::increment_trip();
        tracing::warn!(
            model_id = %key.model_id,
            scope = ?key.scope,
            consecutive_failures = state.consecutive_failures,
            cooldown_secs = cooldown.as_secs(),
            disable_ttl_trips = state.disable_ttl_trips,
            trips_in_window = state.trips_in_window(now),
            hard_disabled,
            "model circuit-breaker tripped on stall (failing over to next model)"
        );
        if hard_disabled {
            tracing::error!(
                model_id = %key.model_id,
                scope = ?key.scope,
                trips_in_window = TRIP_RATE_CEILING,
                window_hours = TRIP_RATE_WINDOW.as_secs() / 3600,
                "model breaker hit trip-rate ceiling on stalls — HARD-DISABLED until a \
                 human re-enables it via model_health(enable)"
            );
        }
    }

    /// Returns `true` when the `(scope, model)` bucket is not circuit-breaker
    /// disabled (or when its cooldown has expired).
    pub fn is_available(&self, scope: Option<&str>, model_id: &str) -> bool {
        let now = SystemClock::new().now_instant();
        let map = self.inner.lock().unwrap();
        map.get(&HealthKey::new(scope, model_id))
            .is_none_or(|s| s.is_available(now))
    }

    /// Returns `true` when the `(scope, model)` bucket is currently demoted by a
    /// **throttle** cooldown — the breaker tripped on a `record_stall(escalate=
    /// false)` quota signal and has not yet been cleared by a success.
    ///
    /// Unlike [`is_available`], this stays `true` through the *half-open* window
    /// (cooldown deadline elapsed but no success has re-proven the model): a
    /// quota that just flapped 5 minutes ago is likely still dead, so dispatch
    /// deprioritizes it behind healthy lane-mates rather than re-picking it
    /// head-of-line and burning a session on another <10s 429 crash. A genuine
    /// (non-throttle) breaker trip is NOT reported here — those are handled by
    /// the ordinary `is_available` skip and their own escalation ladder.
    pub fn is_throttle_cooling(&self, scope: Option<&str>, model_id: &str) -> bool {
        let map = self.inner.lock().unwrap();
        map.get(&HealthKey::new(scope, model_id))
            .is_some_and(|s| s.auto_disabled && s.last_trip_throttle)
    }

    /// Return health state for all tracked buckets, sorted by `(scope, model)`.
    pub fn all_health(&self) -> Vec<ModelHealth> {
        let now = SystemClock::new().now_instant();
        let map = self.inner.lock().unwrap();
        let mut health: Vec<_> = map.iter().map(|(key, s)| s.to_health(key, now)).collect();
        health.sort_by(|a, b| {
            a.scope
                .cmp(&b.scope)
                .then_with(|| a.model_id.cmp(&b.model_id))
        });
        health
    }

    pub fn debug_snapshot(&self) -> Vec<BreakerDebugEntry> {
        let entries: Vec<_> = {
            let map = self.inner.lock().unwrap();
            map.iter()
                .map(|(key, state)| {
                    (
                        key.clone(),
                        state.auto_disabled,
                        state.hard_disabled,
                        state.cooldown_until,
                        state.consecutive_failures,
                    )
                })
                .collect()
        };

        let now = SystemClock::new().now_instant();
        let wall_now = ::time::OffsetDateTime::now_utc();
        let mut snapshot: Vec<_> = entries
            .into_iter()
            .filter_map(
                |(key, auto_disabled, hard_disabled, cooldown_until, consecutive_failures)| {
                    let state = if hard_disabled {
                        "hard_disabled"
                    } else if !auto_disabled {
                        return None;
                    } else if cooldown_until.is_some_and(|until| now >= until) {
                        "half_open"
                    } else {
                        "open"
                    };
                    let until = cooldown_until.map(|deadline| {
                        let wall = if deadline >= now {
                            wall_now + (deadline - now)
                        } else {
                            wall_now - now.duration_since(deadline)
                        };
                        wall.format(&::time::format_description::well_known::Rfc3339)
                            .unwrap_or_else(|_| wall.to_string())
                    });
                    Some(BreakerDebugEntry {
                        scope: key.scope,
                        model: key.model_id,
                        state: state.to_owned(),
                        until,
                        consecutive_failures,
                    })
                },
            )
            .collect();
        snapshot.sort_by(|a, b| a.scope.cmp(&b.scope).then_with(|| a.model.cmp(&b.model)));
        snapshot
    }

    /// Return owned breaker metric snapshots for all tracked buckets.
    ///
    /// This method is synchronous/non-async and does not expose the internal
    /// mutex guard, so callers can safely collect a snapshot before rendering
    /// metrics without holding locks across `.await` points. State maps to the
    /// `djinn_breaker_state{scope,model}` gauge as Closed=`0.0`, HalfOpen=`0.5`,
    /// Open=`1.0`.
    pub fn breaker_metric_snapshot(&self) -> Vec<BreakerMetricSnapshot> {
        let now = SystemClock::new().now_instant();
        let map = self.inner.lock().unwrap();
        let mut snapshot: Vec<_> = map
            .iter()
            .map(|(key, state)| {
                let breaker_state = if !state.auto_disabled {
                    BreakerState::Closed
                } else if state.is_available(now) {
                    BreakerState::HalfOpen
                } else {
                    BreakerState::Open
                };
                BreakerMetricSnapshot {
                    scope: key.scope.clone().unwrap_or_else(|| "shared".to_owned()),
                    model: key.model_id.clone(),
                    state: breaker_state,
                    value: breaker_state.metric_value(),
                }
            })
            .collect();
        snapshot.sort_by(|a, b| a.scope.cmp(&b.scope).then_with(|| a.model.cmp(&b.model)));
        snapshot
    }

    /// Emit scrape-time breaker-state gauges from a lock-free owned snapshot.
    pub fn record_breaker_metrics(&self) {
        for bucket in self.breaker_metric_snapshot() {
            djinn_telemetry::breaker::set_state(&bucket.scope, &bucket.model, bucket.value);
        }
    }

    /// Replace all tracked health state with a persisted snapshot.
    pub fn restore_all(&self, snapshot: Vec<ModelHealth>) {
        let now = SystemClock::new().now_instant();
        let mut map = self.inner.lock().unwrap();
        map.clear();
        for health in snapshot {
            let mut state = ModelState {
                auto_disabled: health.auto_disabled,
                cooldown_until: None,
                consecutive_failures: health.consecutive_failures,
                breaker_eligible_consecutive_failures: health.breaker_eligible_consecutive_failures,
                total_failures: health.total_failures,
                total_successes: health.total_successes,
                disable_ttl_trips: health.disable_ttl_trips,
                hard_disabled: health.hard_disabled,
                // Seed the rolling window with the persisted count so the
                // hard-disable ceiling survives a leader failover / restart
                // instead of resetting to zero. Timestamps are unknown across
                // the persist boundary, so anchor them at `now`: they age out
                // naturally over `TRIP_RATE_WINDOW`.
                trip_times: vec![now; (health.trips_in_window as usize).min(TRIP_RATE_CEILING)],
                // Persistent-throttle streak counters are runtime-only; a
                // restart restarts any in-progress streak from zero.
                ..Default::default()
            };

            if health.hard_disabled {
                // A hard-disable has no auto-expiry — keep it disabled with no
                // cooldown deadline regardless of the persisted remaining seconds.
                state.auto_disabled = true;
                state.cooldown_until = None;
            } else if health.auto_disabled {
                if let Some(seconds) = health.cooldown_seconds_remaining {
                    if seconds > 0 {
                        state.cooldown_until = Some(now + Duration::from_secs(seconds));
                    } else {
                        state.auto_disabled = false;
                    }
                } else {
                    state.auto_disabled = false;
                }
            }

            map.insert(
                HealthKey::new(health.scope.as_deref(), &health.model_id),
                state,
            );
        }
    }

    /// Return health state for a single `(scope, model)` bucket (returns zero
    /// state if untracked).
    pub fn model_health(&self, scope: Option<&str>, model_id: &str) -> ModelHealth {
        let now = SystemClock::new().now_instant();
        let key = HealthKey::new(scope, model_id);
        let map = self.inner.lock().unwrap();
        map.get(&key)
            .map(|s| s.to_health(&key, now))
            .unwrap_or(ModelHealth {
                model_id: model_id.to_owned(),
                scope: scope.map(str::to_owned),
                auto_disabled: false,
                consecutive_failures: 0,
                breaker_eligible_consecutive_failures: 0,
                total_failures: 0,
                total_successes: 0,
                disable_ttl_trips: 0,
                cooldown_seconds_remaining: None,
                hard_disabled: false,
                trips_in_window: 0,
            })
    }

    /// Reset failure/success counters and re-enable a single bucket.
    pub fn reset(&self, scope: Option<&str>, model_id: &str) {
        let mut map = self.inner.lock().unwrap();
        map.insert(HealthKey::new(scope, model_id), ModelState::default());
    }

    /// Reset all tracked buckets.
    pub fn reset_all(&self) {
        let mut map = self.inner.lock().unwrap();
        map.clear();
    }

    /// Reset every bucket owned by `scope`. Used for auto-resume: when a user
    /// reconnects a revoked credential, give all of their models another chance
    /// immediately instead of waiting out the breaker cooldown. Returns the
    /// number of buckets hit.
    pub fn reset_scope(&self, scope: Option<&str>) -> usize {
        let mut map = self.inner.lock().unwrap();
        let target = scope.map(|s| s.to_string());
        let keys: Vec<HealthKey> = map.keys().filter(|k| k.scope == target).cloned().collect();
        for k in &keys {
            map.remove(k);
        }
        keys.len()
    }

    /// Reset every scope's bucket for `model_id` (ops convenience: "wipe this
    /// model's breaker state for everyone"). Returns the number of buckets hit.
    pub fn reset_model_all_scopes(&self, model_id: &str) -> usize {
        let mut map = self.inner.lock().unwrap();
        let keys: Vec<HealthKey> = map
            .keys()
            .filter(|k| k.model_id == model_id)
            .cloned()
            .collect();
        for k in &keys {
            map.remove(k);
        }
        keys.len()
    }

    /// Re-enable an auto-disabled (or **hard-disabled**) bucket without clearing
    /// failure/success counters. This is the human re-enable path a hard-disable
    /// requires, so it also clears the trip-rate window: otherwise the ceiling
    /// would still be tripped and the very next failure would immediately
    /// re-hard-disable the bucket.
    pub fn enable(&self, scope: Option<&str>, model_id: &str) {
        let mut map = self.inner.lock().unwrap();
        let state = map.entry(HealthKey::new(scope, model_id)).or_default();
        state.auto_disabled = false;
        state.hard_disabled = false;
        state.cooldown_until = None;
        state.trip_times.clear();
    }

    /// Re-enable every scope's bucket for `model_id` without clearing failure
    /// counters (ops convenience: "let everyone use this model again now"). Also
    /// clears any hard-disable + the trip-rate window (see [`Self::enable`]).
    /// Returns the number of buckets re-enabled.
    pub fn enable_model_all_scopes(&self, model_id: &str) -> usize {
        let mut map = self.inner.lock().unwrap();
        let mut n = 0;
        for (key, state) in map.iter_mut() {
            if key.model_id == model_id {
                state.auto_disabled = false;
                state.hard_disabled = false;
                state.cooldown_until = None;
                state.trip_times.clear();
                n += 1;
            }
        }
        n
    }
}

impl Default for HealthTracker {
    fn default() -> Self {
        Self::new()
    }
}

// Tests live in a sibling file (`health_tests.rs`) to keep this module under the
// repo source-size guard. It is still a child module of `health`, so it can
// reach these private items via `use super::*`.
#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test: real time for timing assertions
#[path = "health_tests.rs"]
mod tests;
