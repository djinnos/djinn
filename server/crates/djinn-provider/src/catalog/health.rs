// djinn:allow-oversize — per-(scope, model) breaker state: typed-failure, throttle, stall and transient ladders share one mutex-guarded catalog.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use djinn_core::clock::{Clock, SystemClock};
use serde::{Deserialize, Serialize};

/// Number of consecutive failures before circuit breaker trips.
const CIRCUIT_BREAKER_THRESHOLD: u32 = 3;

/// Number of consecutive **transient upstream** faults
/// ([`HealthTracker::record_transient_failure`]) before the breaker trips.
///
/// Deliberately far above [`CIRCUIT_BREAKER_THRESHOLD`], because a transient
/// upstream fault is a *load* signal, not a *model-health* signal. A provider
/// answering `server_is_overloaded` (HTTP 500/502/503/504) or dropping a stream
/// mid-flight says nothing about whether the model can do the work — the
/// identical request succeeds minutes later. Counting those on the ordinary
/// three-strike ladder means an upstream capacity blip demotes the user's
/// preferred model and, via the escalating cooldown, keeps it demoted long after
/// the provider recovered.
///
/// Incident (2026-07-29, task `nr41`): a run of OpenAI `server_is_overloaded`
/// 500s pushed `openai/gpt-5.6-sol` to `auto_disabled: true` at 15 consecutive
/// failures with 6 disable-TTL trips — 30 total failures against 6 successes —
/// which is the tribunal's own adversary model being disabled for someone else's
/// outage. At this threshold that run does not trip the breaker at all.
///
/// It is still *finite*, and that is deliberate: a model whose backend is
/// permanently gone (the kimi-for-coding/`k2p7` signature — instant transport
/// death, zero tokens, re-dispatched forever) must eventually be demoted rather
/// than re-selected indefinitely. Twenty consecutive transient faults with no
/// intervening success is no longer "the provider is busy"; it is a dead
/// endpoint.
const TRANSIENT_BREAKER_THRESHOLD: u32 = 20;
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

/// Base **quarantine** for a hard-disabled bucket: how long after its most
/// recent trip the breaker waits before admitting a single *half-open probe*.
///
/// ## Why this exists (incident, 2026-08-12 → 2026-08-16)
///
/// [`TRIP_RATE_CEILING`] is a rate-based trigger that used to have a permanent,
/// never-re-evaluated consequence: `hard_disabled` was latched and
/// `is_available` returned `false` forever, with no code path other than a human
/// `model_health(enable)` able to clear it. `openai/gpt-5.6-terra` — the only
/// candidate in a user's `implement` lane — latched during a transient provider
/// incident on 08-12 and stayed latched for four days, so every dispatch went
/// `breaker_open` → `failover_chain_exhausted` → cooldown, forever. The
/// autonomous build loop was dead the entire time. When inspected the bucket
/// read `hard_disabled: true` with **`trips_in_window: 0`**: every trip that
/// justified the latch had already aged out of [`TRIP_RATE_WINDOW`], and nothing
/// ever reconsidered. A manual reset showed 10/10 successes immediately.
///
/// A circuit breaker that can never probe is not a breaker, it is a fuse only a
/// human can replace. This constant is the quarantine before the *one* probe.
///
/// ## Why exactly six hours
///
/// Three independent bounds pin this number, and the compile-time assertions
/// below enforce two of them:
///
/// 1. **It must exceed [`MAX_COOLDOWN`] (4h).** The hard-disable exists because
///    "even a 4h ceiling still lets a hopeless model flap a couple of times a
///    day". A quarantine at or below the escalating ladder's own ceiling would
///    make the hard-disable indistinguishable from an ordinary cooldown and
///    reintroduce exactly the pathology it was added to stop.
/// 2. **It equals [`TRIP_RATE_WINDOW`], and that equality is load-bearing.**
///    The deadline is anchored at the bucket's *most recent trip*, so when the
///    probe is finally admitted every trip that justified the latch is at least
///    `TRIP_RATE_WINDOW` old and has therefore been pruned: `trips_in_window`
///    is provably `0` at probe time. The probe asks "is this model healthy
///    *now*" against a window that no longer contains a single piece of the
///    evidence for the latch — which is precisely the state the incident bucket
///    was sitting in, ignored, for four days. Shorter, and the probe fires while
///    the latch is still justified by live evidence; longer, and the breaker
///    keeps punishing a model for evidence it has already discarded.
/// 3. **The exposure it buys is negligible.** At most ONE task is exposed per
///    quarantine period (see [`HealthTracker::note_dispatch_accepted`]), so
///    tier 0 costs at most 4 probe dispatches a day — against the ~50
///    trips/night the original flap incident sustained, and against the 8
///    consecutive 429 crashes that cost one production task 5.75h. Each failed
///    probe then doubles the wait, so a genuinely-dead model converges on one
///    probe a week.
///
/// This is deliberately *not* written as `TRIP_RATE_WINDOW` (an alias would let
/// a future edit to the trip window silently move the quarantine); the
/// relationship is asserted instead.
const HARD_DISABLE_QUARANTINE_BASE: Duration = Duration::from_secs(6 * 60 * 60);

/// Ceiling of the escalating quarantine ladder: **7 days**.
///
/// Each failed probe doubles the quarantine (6h → 12h → 24h → 48h → 96h → 7d),
/// so a model whose backend is genuinely gone converges on one exposed task per
/// week — cheap enough to be irrelevant to the fleet, while still guaranteeing
/// that a model whose provider eventually comes back recovers *without* a human.
/// A week is also comfortably past the point where an operator has noticed, so
/// the ceiling is about not permanently giving up rather than about throughput.
const HARD_DISABLE_QUARANTINE_MAX: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Highest quarantine tier, i.e. the number of doublings needed to reach
/// [`HARD_DISABLE_QUARANTINE_MAX`]. Clamping the *tier* (rather than only the
/// resulting `Duration`) keeps [`quarantine_for_tier`]'s loop bounded no matter
/// how many probe failures accumulate.
const HARD_DISABLE_MAX_PROBE_TIER: u32 = 5;

const _: () = assert!(
    HARD_DISABLE_QUARANTINE_BASE.as_secs() > MAX_COOLDOWN.as_secs(),
    "the hard-disable quarantine must outlast the escalating cooldown ceiling, \
     or the hard-disable adds nothing over an ordinary cooldown and the flap returns"
);
const _: () = assert!(
    HARD_DISABLE_QUARANTINE_BASE.as_secs() == TRIP_RATE_WINDOW.as_secs(),
    "the quarantine is anchored at the last trip so that `trips_in_window` is \
     provably 0 when the half-open probe fires; that invariant requires equality"
);
const _: () = assert!(
    HARD_DISABLE_QUARANTINE_BASE.as_secs() << HARD_DISABLE_MAX_PROBE_TIER
        >= HARD_DISABLE_QUARANTINE_MAX.as_secs(),
    "HARD_DISABLE_MAX_PROBE_TIER must be large enough to actually reach the ceiling"
);

/// Quarantine length for an escalation `tier`: [`HARD_DISABLE_QUARANTINE_BASE`]
/// doubled once per prior failed probe, capped at [`HARD_DISABLE_QUARANTINE_MAX`].
fn quarantine_for_tier(tier: u32) -> Duration {
    let mut quarantine = HARD_DISABLE_QUARANTINE_BASE;
    for _ in 0..tier.min(HARD_DISABLE_MAX_PROBE_TIER) {
        quarantine = (quarantine * 2).min(HARD_DISABLE_QUARANTINE_MAX);
    }
    quarantine
}

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
    /// [`TRIP_RATE_WINDOW`]) was hit, so the bucket is **quarantined** — held
    /// unavailable except for a single half-open probe dispatch admitted once
    /// per (escalating) quarantine period, until either that probe succeeds or a
    /// human re-enables it. `#[serde(default)]` keeps older persisted snapshots
    /// (which had no such field) loading as not-hard-disabled.
    #[serde(default)]
    pub hard_disabled: bool,
    /// Escalation tier of the hard-disable quarantine ladder: `0` at the moment
    /// of the latch, incremented by every failed half-open probe, clamped at
    /// [`HARD_DISABLE_MAX_PROBE_TIER`]. Persisted so a server restart cannot
    /// demote a long-quarantined dead model back to the 6h base and start
    /// probing it four times a day. `#[serde(default)]` for back-compat with
    /// pre-half-open snapshots, which load at tier 0 (the base quarantine).
    #[serde(default)]
    pub hard_disable_probe_tier: u32,
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
    /// Consecutive TRANSIENT upstream faults (5xx / transport death) with no
    /// intervening success, counted on their own much longer ladder
    /// ([`TRANSIENT_BREAKER_THRESHOLD`]) instead of the three-strike one.
    ///
    /// Kept separate from `breaker_eligible_consecutive_failures` so that an
    /// upstream capacity blip cannot demote a healthy model, while a genuinely
    /// dead endpoint still eventually trips. Reset to 0 by any success, and by
    /// any genuine (`record_failure`) trip — once the breaker has fired for a
    /// real fault, this streak is moot. Runtime-only, like the throttle streak.
    transient_consecutive_failures: u32,
    total_failures: u32,
    total_successes: u32,
    disable_ttl_trips: u32,
    /// Hard-disabled (quarantined) by the trip-rate ceiling. Distinct from
    /// `auto_disabled` (which self-heals on a cooldown deadline): while this is
    /// set, availability is governed **solely** by `probe_available_at`, and the
    /// only way back to `false` is a successful half-open probe or a human
    /// re-enable.
    hard_disabled: bool,
    /// Deadline after which a hard-disabled bucket admits exactly ONE half-open
    /// probe dispatch. Anchored at the bucket's most recent trip (not at the
    /// moment the latch was set), so a bucket that keeps failing its probes
    /// keeps pushing its own deadline out. `None` whenever `hard_disabled` is
    /// `false`; always `Some` while it is `true`.
    ///
    /// Consuming the probe (`note_dispatch_accepted`) pushes this forward by a
    /// full quarantine *immediately*, before the probe's verdict is known. That
    /// is what bounds exposure to at most one task per quarantine period even
    /// when the verdict never arrives (task killed, leader failover, pod OOM):
    /// the breaker fails closed rather than re-opening the lane.
    probe_available_at: Option<Instant>,
    /// Escalation tier for `probe_available_at`; see [`quarantine_for_tier`].
    /// Incremented once per *failed* probe and clamped at
    /// [`HARD_DISABLE_MAX_PROBE_TIER`]. Persisted (unlike the throttle streak)
    /// because losing it across a restart would reset a week-long quarantine on
    /// a dead model back to the 6h base.
    hard_disable_probe_tier: u32,
    /// Whether a half-open probe has been handed out and has not yet been
    /// resolved. Set by `note_dispatch_accepted`, cleared by the first
    /// failure/success that follows. Its only job is to make the tier escalate
    /// **once per probe** rather than once per failure record — a single bad
    /// session can produce both a stall and a failure. Runtime-only: a restart
    /// drops it, which at worst costs one un-escalated quarantine, and
    /// `restore_all` re-anchors the deadline anyway.
    probe_outstanding: bool,
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
        // Hard-disabled (quarantined) buckets are unavailable EXCEPT inside the
        // half-open probe window: once `HARD_DISABLE_QUARANTINE_BASE` (doubling
        // per failed probe) has elapsed since the most recent trip, exactly one
        // dispatch is admitted so the breaker can find out whether the model
        // recovered. `note_dispatch_accepted` pushes the deadline forward the
        // moment that dispatch is accepted, so this window closes again after a
        // single task rather than re-opening the lane. This is the whole
        // difference between a breaker and a fuse: the pre-2026-08-16 code
        // returned a flat `false` here and a transient provider incident became
        // permanent configuration state that outlived its cause by four days.
        if self.hard_disabled {
            return matches!(self.probe_available_at, Some(at) if now >= at);
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
            // No ordinary cooldown: clear the deadline so `is_available` relies
            // solely on the quarantine gate below.
            self.cooldown_until = None;
            self.hard_disable_probe_tier = 0;
            self.probe_available_at = Some(now + quarantine_for_tier(0));
            self.probe_outstanding = false;
            return true;
        }
        false
    }

    /// Resolve a hard-disabled bucket's half-open probe as **failed**, or —
    /// when no probe was outstanding — simply re-anchor the quarantine at this
    /// fresh piece of bad evidence.
    ///
    /// Escalation is charged once per *probe*, not once per failure record: a
    /// single doomed session can emit both a stall and a typed failure, and
    /// double-charging would jump a model from the 6h base to the 7d ceiling on
    /// one bad dispatch. Either way the deadline is pushed to
    /// `now + quarantine_for_tier(tier)`, which is what "measured from the last
    /// trip" means.
    fn fail_probe(&mut self, now: Instant) {
        if self.probe_outstanding {
            self.probe_outstanding = false;
            self.hard_disable_probe_tier = self
                .hard_disable_probe_tier
                .saturating_add(1)
                .min(HARD_DISABLE_MAX_PROBE_TIER);
        }
        self.probe_available_at = Some(now + quarantine_for_tier(self.hard_disable_probe_tier));
        // Stay latched, with no ordinary cooldown deadline: while hard-disabled,
        // `is_available` reads `probe_available_at` and nothing else.
        self.auto_disabled = true;
        self.cooldown_until = None;
        // `trip_times` is deliberately NOT touched here. It exists solely to
        // detect the ceiling crossing, and once the bucket is latched that job
        // is done — the quarantine ladder owns the bucket instead. Pushing
        // probe failures into it would also miscount, because a single latching
        // `trip_breaker` burst delivers several failure records in a row.
    }

    /// Release the hard-disable latch. Used by a successful half-open probe and
    /// by the human `enable`/`reset` controls. Also clears the ordinary
    /// auto-disable, because a quarantined bucket carries `auto_disabled: true`
    /// with `cooldown_until: None` — a state `is_available` would otherwise read
    /// as a never-expiring cooldown.
    fn clear_hard_disable(&mut self) {
        self.hard_disabled = false;
        self.probe_available_at = None;
        self.hard_disable_probe_tier = 0;
        self.probe_outstanding = false;
        self.auto_disabled = false;
        self.cooldown_until = None;
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
            hard_disable_probe_tier: self.hard_disable_probe_tier,
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
    /// The failure was a TRANSIENT provider-side fault — a 5xx
    /// (`server_error` / `server_is_overloaded`) or a hard transport death
    /// (`ProviderFailureClass::Transient`).
    ///
    /// Like `throttle`, this is a fault of the provider rather than of the task:
    /// the same transcript redispatched onto a healthy backend succeeds. The
    /// coordinator therefore spares BOTH task-blaming counters for it — the
    /// third-strike planner-remediation `provider_failure_streak` and the
    /// terminal `dispatch_failure_streak` — while the escalating redispatch
    /// cooldown and the per-`(scope, model)` breaker failover still apply.
    ///
    /// Kept as a second flag rather than folding the pair into an enum so the
    /// change stays additive at every call site; `throttle` and `transient` are
    /// mutually exclusive in practice (a class maps to at most one of them).
    /// Incident: task `2gq7`, 2026-07-29 — three independent OpenAI 500s were
    /// indistinguishable from a reproducible task fault, so the third one minted
    /// a bogus "Planner remediation" task.
    pub transient: bool,
    /// Provider-stated reset window (`Retry-After` / rate-limit-reset), if any.
    /// A6: floors the escalating redispatch cooldown so a multi-hour quota
    /// window isn't probed on the fixed ladder. Meaningful for throttles and,
    /// when a provider states one, for transient faults.
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
    /// The time source every breaker transition reads.
    ///
    /// Cooldown deadlines, half-open reclassification and the rolling
    /// trip-rate window are all *time* predicates, so without a seam here the
    /// only way to test them is to race the wall clock: a test that trips the
    /// breaker and then reads it back is really asserting that the five-second
    /// [`INITIAL_COOLDOWN`] outlasts whatever else the test does in between.
    /// That is the mechanism behind the `debug_dispatch_state` and `/metrics`
    /// wedge-fixture flakes — under a loaded runner the cooldown truthfully
    /// expired mid-test and `debug_snapshot` correctly reported `half_open`.
    ///
    /// Held inside the shared handle (not per-clone) so every clone of a
    /// tracker observes the same clock: `AppState` hands clones to the
    /// coordinator, the metrics scraper and the debug endpoint, and they must
    /// agree on what time it is.
    clock: Arc<dyn Clock>,
}

impl HealthTracker {
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock::new()))
    }

    /// Like [`new`](Self::new), but every breaker transition reads the supplied
    /// clock instead of the system one — the deterministic-test seam.
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            task_failures: Arc::new(Mutex::new(HashMap::new())),
            clock,
        }
    }

    /// The single monotonic read for this tracker. Every cooldown deadline,
    /// availability check and trip-window prune goes through here.
    fn now(&self) -> Instant {
        self.clock.now_instant()
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

    /// Read the most recent provider-failure signal for a task **without**
    /// clearing it.
    ///
    /// [`take_task_provider_failure`](Self::take_task_provider_failure) is the
    /// dispatch-reappearance reader and is deliberately consuming, so exactly
    /// one redispatch decision can be made per failure. Two other observers need
    /// the same fact and must not steal it from that reader (nor from each
    /// other):
    ///
    /// * session-exit liveness classification, which must not convict a session
    ///   of a protocol violation when the provider, not the session, died; and
    /// * the refinement tribunal loop, which must park and retry a round rather
    ///   than count it as a completed (dry) round.
    ///
    /// Neither owns the signal, so both peek. The refinement loop clears it
    /// explicitly once it has parked the round, because a refinement task is
    /// force-closed rather than redispatched and would otherwise leak the entry.
    pub fn peek_task_provider_failure(&self, task_id: &str) -> Option<TaskFailureSignal> {
        self.task_failures.lock().unwrap().get(task_id).copied()
    }

    /// Drop a task's provider-failure signal without reading it. For owners that
    /// terminate the task themselves (the refinement loop force-closes its round
    /// tasks), so the side-channel does not accumulate entries for task ids that
    /// will never reappear on the dispatch path.
    pub fn clear_task_provider_failure(&self, task_id: &str) {
        self.task_failures.lock().unwrap().remove(task_id);
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
        let now = self.now();
        let mut map = self.inner.lock().unwrap();
        let key = HealthKey::new(scope, model_id);
        let Some(state) = map.get_mut(&key) else {
            return;
        };

        state.breaker_eligible_consecutive_failures = state
            .breaker_eligible_consecutive_failures
            .saturating_add(1);

        // Half-open probe verdict. While a bucket is hard-disabled the ONLY
        // dispatch that can reach it is the single probe the quarantine
        // admitted, so any failure here is that probe failing: re-latch, double
        // the quarantine and return. Falling through to the ordinary ladder
        // would be wrong twice over — it would need three strikes to react at
        // all, and it would log a second, phantom "breaker tripped".
        if state.hard_disabled {
            state.fail_probe(now);
            tracing::warn!(
                model_id = %key.model_id,
                scope = ?key.scope,
                probe_tier = state.hard_disable_probe_tier,
                next_probe_in_secs = quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                "half-open probe of a hard-disabled model failed — re-quarantined"
            );
            return;
        }

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
        let now = self.now();
        let mut map = self.inner.lock().unwrap();
        let state = map.entry(HealthKey::new(scope, model_id)).or_default();
        state.consecutive_failures = 0;
        state.breaker_eligible_consecutive_failures = 0;
        state.transient_consecutive_failures = 0;
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
        // A success on a quarantined bucket means the half-open probe came back
        // clean: release the latch. This is the only automatic exit from a
        // hard-disable, and it is deliberately not gated on `probe_outstanding`
        // — a productive session is direct positive evidence about the model
        // whichever path produced it, and a genuinely-dead model cannot
        // manufacture one. `clear_hard_disable` also drops `auto_disabled` /
        // `cooldown_until`, which the guard below could not: a quarantined
        // bucket has `auto_disabled: true` with no cooldown deadline, so
        // `is_available` reads `false` and the guard would never fire.
        if state.hard_disabled {
            state.clear_hard_disable();
            tracing::info!(
                model_id = %model_id,
                scope = ?scope,
                "half-open probe succeeded — hard-disable quarantine released"
            );
        }
        if state.auto_disabled && state.is_available(now) {
            state.auto_disabled = false;
            state.cooldown_until = None;
        }
    }

    /// Tell the breaker that a dispatch to this `(scope, model)` bucket was
    /// **accepted** — a session is about to run on it.
    ///
    /// For a healthy bucket this is a no-op. For a *quarantined* one it consumes
    /// the single half-open probe that [`ModelState::is_available`] just
    /// admitted, pushing the next probe deadline a full quarantine into the
    /// future before the probe's verdict is known.
    ///
    /// Consuming here rather than at the availability check is deliberate:
    /// `is_available` is a pure predicate consulted several times per dispatch
    /// decision (and by diagnostics), whereas this fires exactly once, at the
    /// point a real task is actually exposed to the model. And pushing the
    /// deadline *before* the verdict is what makes the bound unconditional — if
    /// the probe's outcome never arrives (task killed, leader failover, pod
    /// OOM) the bucket stays quarantined for another full period instead of
    /// re-opening the lane. At most one task is exposed per quarantine period,
    /// which is the property that keeps the 2026-07 flap (disable 5 min →
    /// re-enable → grab a slot → crash in <10s → repeat, one production task
    /// losing 5.75h to 8 consecutive 429 crashes) impossible.
    pub fn note_dispatch_accepted(&self, scope: Option<&str>, model_id: &str) {
        let now = self.now();
        let mut map = self.inner.lock().unwrap();
        let Some(state) = map.get_mut(&HealthKey::new(scope, model_id)) else {
            return;
        };
        if !state.hard_disabled {
            return;
        }
        if !state.probe_available_at.is_some_and(|at| now >= at) {
            // Not the probe window — nothing to consume. Reachable only if a
            // caller dispatched to a bucket the breaker said was unavailable.
            return;
        }
        state.probe_outstanding = true;
        state.probe_available_at = Some(now + quarantine_for_tier(state.hard_disable_probe_tier));
        tracing::warn!(
            model_id = %model_id,
            scope = ?scope,
            probe_tier = state.hard_disable_probe_tier,
            trips_in_window = state.trips_in_window(now),
            next_probe_in_secs = quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
            "admitting a single half-open probe dispatch to a hard-disabled model"
        );
    }

    /// Record a failed invocation.  Trips the circuit breaker when the
    /// consecutive failure threshold is reached.
    pub fn record_failure(&self, scope: Option<&str>, model_id: &str) {
        let now = self.now();
        let mut map = self.inner.lock().unwrap();
        let key = HealthKey::new(scope, model_id);
        let state = map.entry(key.clone()).or_default();
        state.consecutive_failures += 1;
        state.breaker_eligible_consecutive_failures += 1;
        state.total_failures += 1;

        // Half-open probe verdict — see `apply_breaker_check_for`.
        if state.hard_disabled {
            state.fail_probe(now);
            tracing::warn!(
                model_id = %key.model_id,
                scope = ?key.scope,
                probe_tier = state.hard_disable_probe_tier,
                next_probe_in_secs = quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                "half-open probe of a hard-disabled model failed — re-quarantined"
            );
            return;
        }

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

    /// Record a **transient upstream** fault — a provider-side 5xx
    /// (`server_is_overloaded` / `server_error`, 502/503/504) or a hard
    /// transport/stream death, i.e. [`crate::catalog::health`]'s view of
    /// `ProviderFailureClass::Transient`.
    ///
    /// Counters move exactly as they do for [`record_failure`] *except* the
    /// breaker-eligible one: the fault is fully visible in `model_health`
    /// (`consecutive_failures`, `total_failures`) but is charged to
    /// `transient_consecutive_failures`, which trips only at
    /// [`TRANSIENT_BREAKER_THRESHOLD`] instead of [`CIRCUIT_BREAKER_THRESHOLD`].
    ///
    /// Why this is not [`record_failure`]: an overloaded upstream is a LOAD
    /// signal, not a model-health signal. Three of them in a row is an ordinary
    /// afternoon at a busy provider, and demoting the user's preferred model for
    /// it — then holding it demoted on the escalating cooldown ladder — is the
    /// 2026-07-29 `nr41` incident, where `openai/gpt-5.6-sol` reached
    /// `auto_disabled: true` with 6 disable-TTL trips off a burst of
    /// `server_is_overloaded` 500s and took the tribunal's adversary role with
    /// it.
    ///
    /// Why this is not [`record_stall`] (the `Throttle` treatment): a stall trips
    /// the breaker IMMEDIATELY with a five-minute floor. That is right for a
    /// quota window — which resets on a clock and where instant failover to
    /// another account/model is the only useful move — but wrong for a load
    /// blip, where the very next dispatch usually succeeds. Routing overload
    /// through `Throttle` would still leave the model `auto_disabled`, just for
    /// a shorter time.
    ///
    /// Genuine failure detection is untouched: `Authentication`,
    /// `InvalidRequest` and `InvalidOutput` continue through
    /// [`record_failure`]/[`record_stall`] and still trip at three strikes.
    pub fn record_transient_failure(&self, scope: Option<&str>, model_id: &str) {
        let now = self.now();
        let mut map = self.inner.lock().unwrap();
        let key = HealthKey::new(scope, model_id);
        let state = map.entry(key.clone()).or_default();
        state.consecutive_failures += 1;
        state.transient_consecutive_failures += 1;
        state.total_failures += 1;

        // Half-open probe verdict — see `apply_breaker_check_for`. A transient
        // upstream fault is a weak signal in general, but a bucket that has
        // already earned a hard-disable does not get the benefit of the doubt:
        // its one probe produced nothing usable, so it waits again.
        if state.hard_disabled {
            state.fail_probe(now);
            tracing::warn!(
                model_id = %key.model_id,
                scope = ?key.scope,
                probe_tier = state.hard_disable_probe_tier,
                next_probe_in_secs = quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                "half-open probe of a hard-disabled model failed (transient) — re-quarantined"
            );
            return;
        }

        // If the previous cooldown expired, clear the flag so we can re-trip.
        if state.auto_disabled && state.is_available(now) {
            state.auto_disabled = false;
            state.cooldown_until = None;
        }

        if !state.auto_disabled
            && state.transient_consecutive_failures >= TRANSIENT_BREAKER_THRESHOLD
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
                transient_consecutive_failures = state.transient_consecutive_failures,
                cooldown_secs = cooldown.as_secs(),
                disable_ttl_trips = state.disable_ttl_trips,
                trips_in_window = state.trips_in_window(now),
                hard_disabled,
                "model circuit-breaker tripped on a sustained run of TRANSIENT upstream faults \
                 — this endpoint is not merely busy"
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
        let now = self.now();
        let mut map = self.inner.lock().unwrap();
        let key = HealthKey::new(scope, model_id);
        let state = map.entry(key.clone()).or_default();
        state.consecutive_failures += 1;
        state.total_failures += 1;

        // Half-open probe verdict — see `apply_breaker_check_for`. Note this
        // runs BEFORE the throttle-streak bookkeeping below: while quarantined
        // the model is not on the throttle ladder at all, it is on the
        // hard-disable quarantine ladder, and only one of the two may own the
        // bucket's next deadline.
        if state.hard_disabled {
            state.fail_probe(now);
            tracing::warn!(
                model_id = %key.model_id,
                scope = ?key.scope,
                probe_tier = state.hard_disable_probe_tier,
                next_probe_in_secs = quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                "half-open probe of a hard-disabled model stalled — re-quarantined"
            );
            return;
        }

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
        let now = self.now();
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
        let now = self.now();
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
                        // A quarantined bucket has no ordinary cooldown; its
                        // deadline is the next half-open probe. Rendering that
                        // as `until` is the point: a `hard_disabled` entry used
                        // to show `until: null`, which is exactly why the
                        // 4-day outage looked like a permanent, uninterpretable
                        // state rather than a wait with an end.
                        if state.hard_disabled {
                            state.probe_available_at
                        } else {
                            state.cooldown_until
                        },
                        state.consecutive_failures,
                    )
                })
                .collect()
        };

        let now = self.now();
        // Wall time comes from the same seam as the monotonic read so the
        // rendered `until` deadline stays consistent with the `open`/`half_open`
        // classification computed from `now` (and so both are test-controlled).
        let wall_now = ::time::OffsetDateTime::from(self.clock.now());
        let mut snapshot: Vec<_> = entries
            .into_iter()
            .filter_map(
                |(key, auto_disabled, hard_disabled, deadline_until, consecutive_failures)| {
                    let state = if hard_disabled {
                        // Distinguish "quarantined, waiting" from "quarantined,
                        // one probe dispatch is admissible right now".
                        if deadline_until.is_some_and(|until| now >= until) {
                            "hard_disabled_probe"
                        } else {
                            "hard_disabled"
                        }
                    } else if !auto_disabled {
                        return None;
                    } else if deadline_until.is_some_and(|until| now >= until) {
                        "half_open"
                    } else {
                        "open"
                    };
                    let until = deadline_until.map(|deadline| {
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
        let now = self.now();
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
        let now = self.now();
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
                // A hard-disable has no ordinary cooldown — keep it disabled
                // with no cooldown deadline regardless of the persisted
                // remaining seconds; availability is the quarantine gate only.
                state.auto_disabled = true;
                state.cooldown_until = None;
                // `Instant`s cannot cross the persist boundary, so the
                // quarantine deadline is re-anchored at `now` using the
                // PERSISTED escalation tier. Two properties fall out, and both
                // matter:
                //
                // * A restart can only ever DELAY the next probe — it charges a
                //   full fresh quarantine — never shorten one and never fire a
                //   probe at boot. A crash-loop or a rapid deploy train
                //   therefore cannot be turned into a probe loop, and a
                //   genuinely-dead model cannot be resurrected into a flap by
                //   restarting the server.
                // * Because the tier survives, a model that has failed its way
                //   out to the 7-day rung stays on the 7-day rung. Dropping the
                //   tier would silently demote it to the 6h base and quadruple
                //   its daily exposure across every deploy.
                //
                // The cost is that a long-quarantined model's remaining wait
                // restarts. That is the safe direction, and at the ceiling it is
                // bounded by one extra week.
                state.hard_disable_probe_tier = health
                    .hard_disable_probe_tier
                    .min(HARD_DISABLE_MAX_PROBE_TIER);
                state.probe_available_at =
                    Some(now + quarantine_for_tier(state.hard_disable_probe_tier));
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
        let now = self.now();
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
                hard_disable_probe_tier: 0,
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

    /// Re-enable an auto-disabled (or **hard-disabled**/quarantined) bucket
    /// without clearing its lifetime `total_failures` / `total_successes`.
    ///
    /// This is the human authority path and it takes effect **immediately** and
    /// unconditionally: the quarantine deadline and escalation tier are dropped
    /// along with both disable flags, so an operator never has to wait out a
    /// half-open quarantine they have already overruled.
    ///
    /// It also clears the trip-rate window and every *streak* counter —
    /// `consecutive_failures`, `breaker_eligible_consecutive_failures`,
    /// `transient_consecutive_failures` and the throttle streak. Clearing the
    /// streaks is a deliberate 2026-08-16 change, not the original behaviour:
    /// `enable` used to clear only the disable flags, leaving
    /// `breaker_eligible_consecutive_failures` at or above
    /// [`CIRCUIT_BREAKER_THRESHOLD`], so the very next failure re-tripped the
    /// breaker on the spot and operators had to follow every `enable` with a
    /// `reset` to make it stick. A re-enable that cannot survive one failure is
    /// not a re-enable. The streaks are transient evidence *about the state the
    /// human just overruled*; the lifetime totals are the audit trail and are
    /// preserved, which is what still distinguishes `enable` from `reset`.
    ///
    /// `disable_ttl_trips` is deliberately **kept**: the operator vouched that
    /// the model is usable now, not that its history never happened, so if it
    /// does trip again the escalating cooldown resumes where it left off. The
    /// first successful session clears it (see [`Self::record_success`]).
    pub fn enable(&self, scope: Option<&str>, model_id: &str) {
        let mut map = self.inner.lock().unwrap();
        let state = map.entry(HealthKey::new(scope, model_id)).or_default();
        Self::apply_enable(state);
    }

    /// Shared body of [`Self::enable`] / [`Self::enable_model_all_scopes`].
    fn apply_enable(state: &mut ModelState) {
        state.clear_hard_disable();
        state.trip_times.clear();
        state.consecutive_failures = 0;
        state.breaker_eligible_consecutive_failures = 0;
        state.transient_consecutive_failures = 0;
        state.consecutive_throttle_trips = 0;
        state.throttle_streak_started_at = None;
        state.last_trip_throttle = false;
    }

    /// Re-enable every scope's bucket for `model_id` without clearing lifetime
    /// totals (ops convenience: "let everyone use this model again now"). Also
    /// clears any hard-disable quarantine, the trip-rate window and the failure
    /// streaks (see [`Self::enable`]). Returns the number of buckets re-enabled.
    pub fn enable_model_all_scopes(&self, model_id: &str) -> usize {
        let mut map = self.inner.lock().unwrap();
        let mut n = 0;
        for (key, state) in map.iter_mut() {
            if key.model_id == model_id {
                Self::apply_enable(state);
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
