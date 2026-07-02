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
    total_failures: u32,
    total_successes: u32,
    disable_ttl_trips: u32,
    /// Hard-disabled by the trip-rate ceiling — no auto-expiry, human re-enable
    /// only. Distinct from `auto_disabled` (which self-heals on cooldown).
    hard_disabled: bool,
    /// Monotonic timestamps of recent breaker trips, pruned to
    /// [`TRIP_RATE_WINDOW`]. When its length reaches [`TRIP_RATE_CEILING`] the
    /// bucket is hard-disabled. Only genuine (escalating) trips are recorded —
    /// account-quota throttles do not count toward the ceiling.
    trip_times: Vec<Instant>,
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

    /// Record a successful invocation.  Resets consecutive failure counter;
    /// clears auto-disable state if the cooldown has expired.
    pub fn record_success(&self, scope: Option<&str>, model_id: &str) {
        let now = SystemClock::new().now_instant();
        let mut map = self.inner.lock().unwrap();
        let state = map.entry(HealthKey::new(scope, model_id)).or_default();
        state.consecutive_failures = 0;
        state.total_successes += 1;
        // A productive session is proof the model recovered — reset the
        // escalating-cooldown tier and clear the rolling trip window so the next
        // failure starts fresh at the base cooldown and the hard-disable ceiling
        // isn't reached by ancient, since-recovered trips.
        state.disable_ttl_trips = 0;
        state.trip_times.clear();
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
        state.total_failures += 1;

        // If the previous cooldown expired, clear the flag so we can re-trip.
        if state.auto_disabled && state.is_available(now) {
            state.auto_disabled = false;
            state.cooldown_until = None;
        }

        if !state.auto_disabled && state.consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
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
    ///   we just don't ratchet the cap.
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

        // Trip immediately, cooldown floored at STALL_MIN_COOLDOWN so it
        // outlasts the task's redispatch cooldown and forces failover.
        let cooldown = state.compute_cooldown().max(STALL_MIN_COOLDOWN);
        state.auto_disabled = true;
        state.cooldown_until = Some(now + cooldown);
        // A throttle resets on a clock, not on model health — don't ratchet the
        // escalating cooldown cap for it (idea 6), and don't count it toward the
        // hard-disable ceiling (a quota-limited account is not a broken model).
        // Genuine failures/stalls do both.
        let hard_disabled = if escalate {
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

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test: real time for timing assertions
mod tests {
    use super::*;

    const TEST_MODEL: &str = "model";
    // Most tests exercise the shared (None) bucket; per-scope isolation has its
    // own dedicated tests below.
    const S: Option<&str> = None;

    fn expire_cooldown(ht: &HealthTracker, scope: Option<&str>, model_id: &str) {
        let mut map = ht.inner.lock().unwrap();
        let state = map.get_mut(&HealthKey::new(scope, model_id)).unwrap();
        state.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
    }

    fn trip_breaker(ht: &HealthTracker, model_id: &str) -> ModelHealth {
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure(S, model_id);
        }
        ht.model_health(S, model_id)
    }

    #[test]
    fn debug_snapshot_returns_only_non_closed_entries() {
        let ht = HealthTracker::new();
        {
            let mut map = ht.inner.lock().unwrap();
            map.insert(
                HealthKey::new(Some("user-open"), "open-model"),
                ModelState {
                    auto_disabled: true,
                    cooldown_until: Some(Instant::now() + Duration::from_secs(60)),
                    consecutive_failures: 3,
                    total_failures: 3,
                    total_successes: 0,
                    disable_ttl_trips: 1,
                    ..Default::default()
                },
            );
            map.insert(
                HealthKey::new(Some("user-half"), "half-model"),
                ModelState {
                    auto_disabled: true,
                    cooldown_until: Some(Instant::now() - Duration::from_secs(1)),
                    consecutive_failures: 4,
                    total_failures: 4,
                    total_successes: 0,
                    disable_ttl_trips: 1,
                    ..Default::default()
                },
            );
            map.insert(
                HealthKey::new(Some("user-closed"), "closed-model"),
                ModelState {
                    auto_disabled: false,
                    cooldown_until: None,
                    consecutive_failures: 0,
                    total_failures: 0,
                    total_successes: 1,
                    disable_ttl_trips: 0,
                    ..Default::default()
                },
            );
        }

        let snapshot = ht.debug_snapshot();
        assert_eq!(snapshot.len(), 2);
        assert!(
            snapshot
                .iter()
                .any(|entry| entry.model == "open-model" && entry.state == "open")
        );
        assert!(
            snapshot
                .iter()
                .any(|entry| entry.model == "half-model" && entry.state == "half_open")
        );
        assert!(!snapshot.iter().any(|entry| entry.model == "closed-model"));
    }

    #[test]
    fn healthy_model_is_available() {
        let ht = HealthTracker::new();
        assert!(ht.is_available(S, "gpt-4o"));
    }

    #[test]
    fn circuit_breaker_trips_at_threshold() {
        let ht = HealthTracker::new();
        let pre_threshold = CIRCUIT_BREAKER_THRESHOLD - 1;

        for _ in 0..pre_threshold {
            ht.record_failure(S, "bad-model");
        }

        let before_trip = ht.model_health(S, "bad-model");
        assert!(ht.is_available(S, "bad-model"));
        assert!(!before_trip.auto_disabled);
        assert_eq!(before_trip.consecutive_failures, pre_threshold);
        assert_eq!(before_trip.total_failures, pre_threshold);
        assert_eq!(before_trip.disable_ttl_trips, 0);
        assert!(before_trip.cooldown_seconds_remaining.is_none());

        ht.record_failure(S, "bad-model");

        assert!(!ht.is_available(S, "bad-model"));
        let h = ht.model_health(S, "bad-model");
        assert!(h.auto_disabled);
        assert_eq!(h.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD);
        assert_eq!(h.total_failures, CIRCUIT_BREAKER_THRESHOLD);
        assert_eq!(h.disable_ttl_trips, 1);
        let remaining = h.cooldown_seconds_remaining.unwrap();
        assert!(remaining <= INITIAL_COOLDOWN.as_secs());
        assert!(remaining >= INITIAL_COOLDOWN.as_secs().saturating_sub(1));
    }

    #[test]
    fn success_resets_consecutive_counter() {
        let ht = HealthTracker::new();
        for _ in 0..(CIRCUIT_BREAKER_THRESHOLD - 1) {
            ht.record_failure(S, TEST_MODEL);
        }
        ht.record_success(S, TEST_MODEL);
        for _ in 0..(CIRCUIT_BREAKER_THRESHOLD - 1) {
            ht.record_failure(S, TEST_MODEL);
        }
        let h = ht.model_health(S, TEST_MODEL);
        assert_eq!(h.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD - 1);
        assert!(!h.auto_disabled);
        assert_eq!(h.total_failures, 2 * (CIRCUIT_BREAKER_THRESHOLD - 1));
        assert_eq!(h.total_successes, 1);
        assert_eq!(h.disable_ttl_trips, 0);
        assert!(ht.is_available(S, TEST_MODEL));
    }

    #[test]
    fn reset_clears_state() {
        let ht = HealthTracker::new();
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure(S, TEST_MODEL);
        }
        ht.reset(S, TEST_MODEL);
        assert!(ht.is_available(S, TEST_MODEL));
        let h = ht.model_health(S, TEST_MODEL);
        assert_eq!(h.total_failures, 0);
    }

    #[test]
    fn enable_re_enables_without_clearing_counters() {
        let ht = HealthTracker::new();
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure(S, TEST_MODEL);
        }
        ht.enable(S, TEST_MODEL);
        assert!(ht.is_available(S, TEST_MODEL));
        let h = ht.model_health(S, TEST_MODEL);
        assert_eq!(h.total_failures, CIRCUIT_BREAKER_THRESHOLD);
    }

    #[test]
    fn success_resets_escalation_tier_and_trip_window() {
        // Fix 1: a productive session on a model that had been escalating must
        // reset both the cooldown tier (`disable_ttl_trips`) and the rolling
        // trip-rate window, so the next failure starts fresh at the base cooldown
        // and old, since-recovered trips don't count toward the hard-disable
        // ceiling.
        let ht = HealthTracker::new();
        for _ in 0..3 {
            trip_breaker(&ht, TEST_MODEL);
            expire_cooldown(&ht, S, TEST_MODEL);
        }
        let escalated = ht.model_health(S, TEST_MODEL);
        assert_eq!(escalated.disable_ttl_trips, 3);
        assert_eq!(escalated.trips_in_window, 3);

        ht.record_success(S, TEST_MODEL);
        let recovered = ht.model_health(S, TEST_MODEL);
        assert_eq!(recovered.disable_ttl_trips, 0, "success resets the tier");
        assert_eq!(recovered.trips_in_window, 0, "success clears the window");
        assert_eq!(recovered.consecutive_failures, 0);

        // The next trip starts back at the base cooldown, not the escalated one.
        let h = trip_breaker(&ht, TEST_MODEL);
        assert_eq!(h.disable_ttl_trips, 1);
        let remaining = h.cooldown_seconds_remaining.unwrap();
        assert!(remaining <= INITIAL_COOLDOWN.as_secs());
        assert!(remaining >= INITIAL_COOLDOWN.as_secs().saturating_sub(1));
    }

    #[test]
    fn trip_rate_ceiling_hard_disables_until_manual_enable() {
        // Fix 1: a model that trips CEILING times within the window is hard-
        // disabled with NO auto-expiry; only the human `enable` path recovers it.
        let ht = HealthTracker::new();
        for _ in 0..TRIP_RATE_CEILING {
            trip_breaker(&ht, TEST_MODEL);
            expire_cooldown(&ht, S, TEST_MODEL);
        }
        let h = ht.model_health(S, TEST_MODEL);
        assert!(h.hard_disabled);
        assert!(h.auto_disabled);
        assert!(h.cooldown_seconds_remaining.is_none());
        assert_eq!(h.trips_in_window, TRIP_RATE_CEILING as u32);
        // No amount of cooldown expiry re-enables a hard-disabled bucket.
        expire_cooldown(&ht, S, TEST_MODEL);
        assert!(!ht.is_available(S, TEST_MODEL));

        // The existing admin `enable` action clears the hard-disable AND the
        // trip window, so the model is usable again and does not instantly
        // re-hard-disable on the next trip.
        ht.enable(S, TEST_MODEL);
        assert!(ht.is_available(S, TEST_MODEL));
        let enabled = ht.model_health(S, TEST_MODEL);
        assert!(!enabled.hard_disabled);
        assert_eq!(enabled.trips_in_window, 0);
        // Counters (total failures) are preserved by enable.
        assert!(enabled.total_failures >= TRIP_RATE_CEILING as u32);
    }

    #[test]
    fn throttle_stalls_never_hit_the_hard_disable_ceiling() {
        // A quota throttle (`escalate = false`) resets on a clock, not on model
        // health, so it must NOT count toward the hard-disable ceiling — even far
        // more than CEILING throttle trips must keep auto-recovering.
        let ht = HealthTracker::new();
        for _ in 0..(TRIP_RATE_CEILING * 3) {
            ht.record_stall(S, TEST_MODEL, false);
            let h = ht.model_health(S, TEST_MODEL);
            assert!(!h.hard_disabled, "throttles never hard-disable");
            assert_eq!(h.trips_in_window, 0, "throttles are not counted");
            assert_eq!(h.disable_ttl_trips, 0);
            expire_cooldown(&ht, S, TEST_MODEL);
            assert!(ht.is_available(S, TEST_MODEL), "throttle always self-heals");
        }
    }

    #[test]
    fn hard_disable_survives_persistence_round_trip() {
        // A hard-disable must persist across a leader failover (settings-blob
        // snapshot → restore) so a restart doesn't silently re-enable a model a
        // human hasn't cleared.
        let ht = HealthTracker::new();
        for _ in 0..TRIP_RATE_CEILING {
            trip_breaker(&ht, TEST_MODEL);
            expire_cooldown(&ht, S, TEST_MODEL);
        }
        assert!(ht.model_health(S, TEST_MODEL).hard_disabled);

        let snapshot = ht.all_health();
        let restored = HealthTracker::new();
        restored.restore_all(snapshot);
        let h = restored.model_health(S, TEST_MODEL);
        assert!(h.hard_disabled, "hard-disable survived the round trip");
        assert!(!restored.is_available(S, TEST_MODEL));
        assert!(
            h.cooldown_seconds_remaining.is_none(),
            "still no auto-expiry"
        );
    }

    #[test]
    fn compute_cooldown_grows_exponentially_and_caps() {
        let mut state = ModelState::default();

        assert_eq!(state.compute_cooldown(), INITIAL_COOLDOWN);

        state.disable_ttl_trips = 1;
        assert_eq!(state.compute_cooldown(), INITIAL_COOLDOWN * 3);

        state.disable_ttl_trips = 2;
        assert_eq!(state.compute_cooldown(), INITIAL_COOLDOWN * 9);

        state.disable_ttl_trips = 3;
        assert_eq!(state.compute_cooldown(), INITIAL_COOLDOWN * 27);

        // 5s·3^n keeps climbing well past the old 5-minute ceiling now that the
        // cap is 4h: 5·3^7 = 10935s (~3h) is still below the cap…
        state.disable_ttl_trips = 7;
        assert_eq!(state.compute_cooldown(), INITIAL_COOLDOWN * 3u32.pow(7));
        assert!(state.compute_cooldown() < MAX_COOLDOWN);

        // …and 5·3^8 = 32805s (~9h) exceeds it, so it pins at MAX_COOLDOWN.
        state.disable_ttl_trips = 8;
        assert_eq!(state.compute_cooldown(), MAX_COOLDOWN);

        state.disable_ttl_trips = 20;
        assert_eq!(state.compute_cooldown(), MAX_COOLDOWN);
    }

    #[test]
    fn repeated_trips_increase_cooldown_and_expired_cooldown_reenables() {
        let ht = HealthTracker::new();

        for (expected_trip, expected_secs) in [(1_u32, 5_u64), (2, 15), (3, 45)] {
            let health = trip_breaker(&ht, TEST_MODEL);
            assert_eq!(health.disable_ttl_trips, expected_trip);
            assert!(health.auto_disabled);
            let remaining = health.cooldown_seconds_remaining.unwrap();
            assert!(remaining <= expected_secs);
            assert!(remaining >= expected_secs.saturating_sub(1));
            assert!(!ht.is_available(S, TEST_MODEL));

            expire_cooldown(&ht, S, TEST_MODEL);
            let expired = ht.model_health(S, TEST_MODEL);
            assert!(!expired.auto_disabled);
            assert!(expired.cooldown_seconds_remaining.is_none());
            assert!(ht.is_available(S, TEST_MODEL));
        }

        let expired = ht.model_health(S, TEST_MODEL);
        assert_eq!(expired.disable_ttl_trips, 3);
        assert_eq!(expired.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD * 3);

        ht.record_success(S, TEST_MODEL);
        let reenabled = ht.model_health(S, TEST_MODEL);
        assert!(!reenabled.auto_disabled);
        assert_eq!(reenabled.consecutive_failures, 0);
    }

    #[test]
    fn repeated_trips_escalate_cooldown_then_hard_disable_at_ceiling() {
        let ht = HealthTracker::new();

        // Trips 1..CEILING escalate the cooldown (5s·3^(n-1)) and self-heal after
        // an expired cooldown — never reaching the multi-hour cap because the
        // hard-disable ceiling backstops first.
        for expected_trip in 1..TRIP_RATE_CEILING as u32 {
            let health = trip_breaker(&ht, TEST_MODEL);
            assert_eq!(health.disable_ttl_trips, expected_trip);
            assert!(
                !health.hard_disabled,
                "below the ceiling, not hard-disabled"
            );
            assert_eq!(health.trips_in_window, expected_trip);
            let remaining = health.cooldown_seconds_remaining.unwrap();
            let expected = (INITIAL_COOLDOWN.as_secs() * 3u64.pow(expected_trip - 1))
                .min(MAX_COOLDOWN.as_secs());
            assert!(remaining <= expected);
            assert!(remaining >= expected.saturating_sub(1));
            // Cooldown auto-expires → the bucket is available again (self-heal).
            expire_cooldown(&ht, S, TEST_MODEL);
            assert!(ht.is_available(S, TEST_MODEL));
        }

        // The CEILING-th trip within the window flips the bucket to hard-disabled:
        // no cooldown deadline, and NOT available even after expiry.
        let health = trip_breaker(&ht, TEST_MODEL);
        assert_eq!(health.disable_ttl_trips, TRIP_RATE_CEILING as u32);
        assert!(
            health.hard_disabled,
            "ceiling trip hard-disables the bucket"
        );
        assert!(health.auto_disabled, "hard-disabled reports as disabled");
        assert!(
            health.cooldown_seconds_remaining.is_none(),
            "no auto-expiry"
        );
        assert!(!ht.is_available(S, TEST_MODEL));
        // Even forcing the (absent) cooldown into the past does not re-enable it.
        expire_cooldown(&ht, S, TEST_MODEL);
        assert!(
            !ht.is_available(S, TEST_MODEL),
            "hard-disable never self-heals"
        );
    }

    #[test]
    fn stall_trips_immediately_without_consecutive_threshold() {
        let ht = HealthTracker::new();
        // A single stall (well below CIRCUIT_BREAKER_THRESHOLD) disables the model.
        ht.record_stall(S, TEST_MODEL, true);
        assert!(!ht.is_available(S, TEST_MODEL));
        let h = ht.model_health(S, TEST_MODEL);
        assert!(h.auto_disabled);
        assert_eq!(h.consecutive_failures, 1);
        assert_eq!(h.total_failures, 1);
        assert_eq!(h.disable_ttl_trips, 1);
    }

    #[test]
    fn stall_cooldown_outlasts_task_redispatch_cooldown() {
        // The coordinator re-dispatches a stalled task after an escalating
        // *task* cooldown: 60s (streak 1) → 120s → 240s. The model cooldown
        // from a stall must exceed those so the model is still unavailable when
        // the task re-dispatches, forcing failover to the next model.
        const MAX_TASK_REDISPATCH_COOLDOWN_SECS: u64 = 240;

        let ht = HealthTracker::new();
        ht.record_stall(S, TEST_MODEL, true);
        let remaining = ht
            .model_health(S, TEST_MODEL)
            .cooldown_seconds_remaining
            .expect("stalled model must be cooling down");
        assert!(
            remaining > MAX_TASK_REDISPATCH_COOLDOWN_SECS,
            "stall cooldown {remaining}s must outlast the {MAX_TASK_REDISPATCH_COOLDOWN_SECS}s task redispatch cooldown"
        );
        // Specifically: floored at the 5-minute cap.
        assert!(remaining <= STALL_MIN_COOLDOWN.as_secs());
        assert!(remaining >= STALL_MIN_COOLDOWN.as_secs().saturating_sub(1));
    }

    #[test]
    fn stall_self_heals_after_cooldown_then_success_resets() {
        let ht = HealthTracker::new();
        ht.record_stall(S, TEST_MODEL, true);
        assert!(!ht.is_available(S, TEST_MODEL));

        // Cooldown expires → model auto-re-enables (one-off stall self-heals).
        expire_cooldown(&ht, S, TEST_MODEL);
        assert!(ht.is_available(S, TEST_MODEL));
        assert!(!ht.model_health(S, TEST_MODEL).auto_disabled);

        // A successful turn resets the consecutive-failure counter.
        ht.record_success(S, TEST_MODEL);
        let h = ht.model_health(S, TEST_MODEL);
        assert_eq!(h.consecutive_failures, 0);
        assert_eq!(h.total_successes, 1);
        assert!(ht.is_available(S, TEST_MODEL));
    }

    #[test]
    fn repeated_stalls_escalate_trips_and_stay_floored() {
        let ht = HealthTracker::new();
        for expected_trip in 1..=4 {
            ht.record_stall(S, TEST_MODEL, true);
            let h = ht.model_health(S, TEST_MODEL);
            assert_eq!(h.disable_ttl_trips, expected_trip);
            assert_eq!(h.trips_in_window, expected_trip);
            assert!(!h.hard_disabled, "4 trips is below the ceiling");
            assert!(h.auto_disabled);
            // The early rungs (5s/15s/45s/135s) are all below the stall floor, so
            // every stall cooldown is floored at STALL_MIN_COOLDOWN (5 min) — no
            // longer aliased to the multi-hour MAX_COOLDOWN.
            let remaining = h.cooldown_seconds_remaining.unwrap();
            assert!(remaining <= STALL_MIN_COOLDOWN.as_secs());
            assert!(remaining >= STALL_MIN_COOLDOWN.as_secs().saturating_sub(1));
            assert!(!ht.is_available(S, TEST_MODEL));
            expire_cooldown(&ht, S, TEST_MODEL);
        }
    }

    #[test]
    fn throttle_stalls_trip_for_failover_without_escalating_the_cap() {
        // Idea 6: a throttle-classified stall (`escalate = false`) — the
        // Codex/OpenAI empty-200 account-quota signal — must still trip the
        // breaker (so dispatch fails over) AND floor the cooldown at the cap (so
        // the model is unavailable past the task redispatch ladder), but it must
        // NOT advance `disable_ttl_trips`: a quota resets on a clock, not on model
        // health, so ratcheting the escalating cooldown cap would be wrong.
        let ht = HealthTracker::new();
        for _ in 1..=4 {
            ht.record_stall(S, TEST_MODEL, false);
            let h = ht.model_health(S, TEST_MODEL);
            // Tripped + floored for failover, exactly like a genuine stall.
            assert!(h.auto_disabled);
            assert!(!ht.is_available(S, TEST_MODEL));
            let remaining = h.cooldown_seconds_remaining.unwrap();
            assert!(remaining <= STALL_MIN_COOLDOWN.as_secs());
            assert!(remaining >= STALL_MIN_COOLDOWN.as_secs().saturating_sub(1));
            // …but the escalation cap counter never advances.
            assert_eq!(
                h.disable_ttl_trips, 0,
                "a throttle stall must not escalate disable_ttl_trips",
            );
            expire_cooldown(&ht, S, TEST_MODEL);
        }
    }

    #[test]
    fn stall_while_cooling_down_does_not_shorten_cooldown() {
        let ht = HealthTracker::new();
        ht.record_stall(S, TEST_MODEL, true);
        let first = ht.model_health(S, TEST_MODEL);
        assert_eq!(first.disable_ttl_trips, 1);

        // A second stall arriving while still cooling down is a no-op for the
        // cooldown window (no re-trip, no reset) — it only bumps failure counts.
        ht.record_stall(S, TEST_MODEL, true);
        let second = ht.model_health(S, TEST_MODEL);
        assert_eq!(second.disable_ttl_trips, 1, "no re-trip while cooling down");
        assert_eq!(second.consecutive_failures, 2);
        assert!(!ht.is_available(S, TEST_MODEL));
    }

    #[test]
    fn scope_isolates_breaker_state_between_users() {
        // The core of the per-user fix: user A stalling a model must NOT disable
        // it for user B, nor for the shared/system bucket.
        let ht = HealthTracker::new();
        let a = Some("user-a");
        let b = Some("user-b");

        ht.record_stall(a, TEST_MODEL, true);

        assert!(!ht.is_available(a, TEST_MODEL), "A's bucket is disabled");
        assert!(ht.is_available(b, TEST_MODEL), "B's bucket is untouched");
        assert!(
            ht.is_available(None, TEST_MODEL),
            "shared/system bucket is untouched"
        );

        // B failing independently does not affect A's already-tripped state.
        ht.record_failure(b, TEST_MODEL);
        assert_eq!(ht.model_health(a, TEST_MODEL).disable_ttl_trips, 1);
        assert_eq!(ht.model_health(b, TEST_MODEL).consecutive_failures, 1);
    }

    #[test]
    fn enable_and_reset_all_scopes_hit_every_bucket() {
        let ht = HealthTracker::new();
        ht.record_stall(Some("user-a"), TEST_MODEL, true);
        ht.record_stall(Some("user-b"), TEST_MODEL, true);
        ht.record_stall(None, TEST_MODEL, true);
        // A different model must be left alone.
        ht.record_stall(Some("user-a"), "other-model", true);

        let re_enabled = ht.enable_model_all_scopes(TEST_MODEL);
        assert_eq!(re_enabled, 3);
        assert!(ht.is_available(Some("user-a"), TEST_MODEL));
        assert!(ht.is_available(Some("user-b"), TEST_MODEL));
        assert!(ht.is_available(None, TEST_MODEL));
        assert!(!ht.is_available(Some("user-a"), "other-model"));

        // enable keeps counters; reset wipes them.
        assert_eq!(
            ht.model_health(Some("user-a"), TEST_MODEL).total_failures,
            1
        );
        let wiped = ht.reset_model_all_scopes(TEST_MODEL);
        assert_eq!(wiped, 3);
        assert_eq!(
            ht.model_health(Some("user-a"), TEST_MODEL).total_failures,
            0
        );
    }

    #[test]
    fn restore_all_rehydrates_disabled_state_without_new_trip() {
        let ht = HealthTracker::new();
        ht.restore_all(vec![ModelHealth {
            model_id: "a/model".to_string(),
            scope: Some("user-a".to_string()),
            auto_disabled: true,
            consecutive_failures: CIRCUIT_BREAKER_THRESHOLD,
            total_failures: 10,
            total_successes: 2,
            disable_ttl_trips: 1,
            cooldown_seconds_remaining: Some(4),
            hard_disabled: false,
            trips_in_window: 1,
        }]);

        let scope = Some("user-a");
        let h = ht.model_health(scope, "a/model");
        assert!(h.auto_disabled);
        assert!(!ht.is_available(scope, "a/model"));
        assert_eq!(h.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD);
        assert_eq!(h.total_failures, 10);
        assert_eq!(h.total_successes, 2);
        assert_eq!(h.disable_ttl_trips, 1);
        // The same model under a different scope is unaffected by the snapshot.
        assert!(ht.is_available(None, "a/model"));
        let remaining = h.cooldown_seconds_remaining.unwrap();
        assert!(remaining <= 4);
        assert!(remaining >= 3);

        ht.enable(scope, "a/model");
        let enabled = ht.model_health(scope, "a/model");
        assert!(ht.is_available(scope, "a/model"));
        assert!(!enabled.auto_disabled);
        assert_eq!(enabled.disable_ttl_trips, 1);
        assert_eq!(enabled.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD);

        ht.record_success(scope, "a/model");
        let restored = ht.model_health(scope, "a/model");
        assert!(ht.is_available(scope, "a/model"));
        assert!(!restored.auto_disabled);
        // A success resets the escalation tier (Fix 1): next failure starts fresh.
        assert_eq!(restored.disable_ttl_trips, 0);
        assert_eq!(restored.consecutive_failures, 0);

        for failures in 1..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure(scope, "a/model");
            let health = ht.model_health(scope, "a/model");
            assert!(ht.is_available(scope, "a/model"));
            assert!(!health.auto_disabled);
            assert_eq!(health.disable_ttl_trips, 0);
            assert_eq!(health.consecutive_failures, failures);
        }

        let before_expiry = ht.model_health(scope, "a/model");
        assert_eq!(before_expiry.disable_ttl_trips, 0);

        ht.restore_all(vec![ModelHealth {
            model_id: "a/model".to_string(),
            scope: Some("user-a".to_string()),
            auto_disabled: true,
            consecutive_failures: CIRCUIT_BREAKER_THRESHOLD,
            total_failures: 10,
            total_successes: 2,
            disable_ttl_trips: 1,
            cooldown_seconds_remaining: Some(2),
            hard_disabled: false,
            trips_in_window: 1,
        }]);
        assert!(!ht.is_available(scope, "a/model"));
        assert_eq!(ht.model_health(scope, "a/model").disable_ttl_trips, 1);

        expire_cooldown(&ht, scope, "a/model");
        let cooled_off = ht.model_health(scope, "a/model");
        assert!(ht.is_available(scope, "a/model"));
        assert!(!cooled_off.auto_disabled);
        assert_eq!(cooled_off.disable_ttl_trips, 1);
        assert!(cooled_off.cooldown_seconds_remaining.is_none());
    }

    #[test]
    fn task_failure_signal_is_read_once_and_cleared() {
        let ht = HealthTracker::new();
        // No signal recorded → nothing to take.
        assert_eq!(ht.take_task_provider_failure("task-1"), None);

        ht.note_task_provider_failure(
            "task-1",
            TaskFailureSignal {
                throttle: true,
                retry_after_ms: Some(5 * 60 * 60 * 1000),
            },
        );
        // The side-channel is independent of the breaker buckets.
        assert!(ht.is_available(S, TEST_MODEL));

        let taken = ht
            .take_task_provider_failure("task-1")
            .expect("signal present");
        assert!(taken.throttle);
        assert_eq!(taken.retry_after_ms, Some(5 * 60 * 60 * 1000));

        // Read-and-clear: a second take returns nothing (no stale leak into a
        // later, unrelated redispatch decision).
        assert_eq!(ht.take_task_provider_failure("task-1"), None);
    }

    #[test]
    fn task_failure_signal_latest_wins_and_is_per_task() {
        let ht = HealthTracker::new();
        ht.note_task_provider_failure(
            "task-a",
            TaskFailureSignal {
                throttle: false,
                retry_after_ms: None,
            },
        );
        // Latest failure for a task overwrites the prior one.
        ht.note_task_provider_failure(
            "task-a",
            TaskFailureSignal {
                throttle: true,
                retry_after_ms: Some(1_000),
            },
        );
        ht.note_task_provider_failure(
            "task-b",
            TaskFailureSignal {
                throttle: false,
                retry_after_ms: None,
            },
        );

        let a = ht.take_task_provider_failure("task-a").unwrap();
        assert!(a.throttle);
        assert_eq!(a.retry_after_ms, Some(1_000));
        // task-b is untouched by reads/writes of task-a.
        let b = ht.take_task_provider_failure("task-b").unwrap();
        assert!(!b.throttle);
    }

    #[test]
    fn breaker_metric_snapshot_covers_closed_half_open_and_open() {
        let ht = HealthTracker::new();
        ht.record_success(Some("user-closed"), "closed-model");
        ht.record_stall(Some("user-half"), "half-model", true);
        expire_cooldown(&ht, Some("user-half"), "half-model");
        ht.record_stall(None, "open-model", true);

        let snapshot = ht.breaker_metric_snapshot();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[0].scope, "shared");
        assert_eq!(snapshot[0].model, "open-model");
        assert_eq!(snapshot[0].state, BreakerState::Open);
        assert_eq!(snapshot[0].value, 1.0);
        assert_eq!(snapshot[1].scope, "user-closed");
        assert_eq!(snapshot[1].state, BreakerState::Closed);
        assert_eq!(snapshot[1].value, 0.0);
        assert_eq!(snapshot[2].scope, "user-half");
        assert_eq!(snapshot[2].state, BreakerState::HalfOpen);
        assert_eq!(snapshot[2].value, 0.5);
    }

    #[test]
    fn breaker_trip_metric_increments_only_on_trip_transition() {
        djinn_telemetry::init().unwrap();
        let before = rendered_counter_value("djinn_breaker_trips_total");
        let ht = HealthTracker::new();

        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure(Some("trip-metric-user"), "trip-metric-model");
        }
        ht.record_failure(Some("trip-metric-user"), "trip-metric-model");
        ht.record_stall(Some("trip-metric-user"), "trip-metric-model", true);

        assert_eq!(
            ht.model_health(Some("trip-metric-user"), "trip-metric-model")
                .disable_ttl_trips,
            1
        );
        assert!(
            rendered_counter_value("djinn_breaker_trips_total") - before >= 1.0,
            "the authoritative closed-to-open transition should increment the breaker-trip counter"
        );
    }

    fn rendered_counter_value(metric: &str) -> f64 {
        let rendered = djinn_telemetry::render().unwrap();
        rendered
            .lines()
            .find_map(|line| {
                let value = line.strip_prefix(metric)?.trim();
                value.parse::<f64>().ok()
            })
            .unwrap_or(0.0)
    }

    #[test]
    fn legacy_snapshot_without_scope_loads_as_shared_bucket() {
        // Snapshots persisted before the per-scope change have no `scope` field;
        // serde defaults it to None → the shared bucket.
        let ht = HealthTracker::new();
        let legacy = serde_json::json!([{
            "model_id": "openai/gpt-5.5",
            "auto_disabled": true,
            "consecutive_failures": 3,
            "total_failures": 3,
            "total_successes": 0,
            "disable_ttl_trips": 1,
            "cooldown_seconds_remaining": 30
        }]);
        let snapshot: Vec<ModelHealth> = serde_json::from_value(legacy).unwrap();
        ht.restore_all(snapshot);
        assert!(!ht.is_available(None, "openai/gpt-5.5"));
        assert_eq!(ht.model_health(None, "openai/gpt-5.5").scope, None);
    }
}
