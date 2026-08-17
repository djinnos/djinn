// djinn:allow-oversize — per-(scope, model) breaker state: typed-failure, throttle, stall and transient ladders share one mutex-guarded catalog.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

/// Successful sessions a bucket on probation must win to have its hard-disable
/// history forgiven, out of a trial of [`HARD_DISABLE_PROBATION_TRIAL_SESSIONS`].
///
/// Probation is the state a bucket enters when a half-open probe SUCCEEDS. The
/// lane reopens at full throughput — that is what fixes the 4-day outage — but
/// the quarantine tier is preserved and the bucket must win a short,
/// hard-bounded trial before its history is written off.
///
/// Deliberately evidence-based rather than clock-based: "prove you work" is the
/// question probation asks, and a wall-clock window would forgive a model that
/// was merely idle.
const HARD_DISABLE_PROBATION_SUCCESSES: u32 = 5;

/// Hard ceiling on how many dispatches a bucket on probation may be handed
/// before it is either forgiven or re-quarantined.
///
/// ## Why probation is counted, not streaked
///
/// The first version of probation re-quarantined on a *trip*, and a trip needs
/// [`CIRCUIT_BREAKER_THRESHOLD`] **consecutive** breaker-eligible failures, with
/// any interleaved success resetting that counter. That made probation defeated
/// by failure CLUSTERING rather than by failure rate: a model that never happens
/// to fail three times in a row was never re-quarantined at all, however bad it
/// was. Measured over 30 simulated days with probation nominally in force:
///
/// ```text
///   one success in 2 → 42840 exposed, never re-quarantined
///   one success in 3 → 42840 exposed, never re-quarantined
///   F,F,S repeating  → 40682 exposed, never re-quarantined
///   one success in 6 →    17 exposed  (the single input the old test asserted)
/// ```
///
/// 42840 is bit-for-bit the number the pre-probation version was rejected for.
/// `origin/main` exposes 0. So the bound has to be a property of the mechanism
/// rather than of the input distribution.
///
/// ## The bound
///
/// Probation is now a fixed trial: **win [`HARD_DISABLE_PROBATION_SUCCESSES`] of
/// at most this many sessions**. Successes and failures are counted
/// independently and neither resets the other, so clustering is irrelevant. The
/// trial resolves in one of three ways, all of them inside this many dispatches:
///
/// * [`HARD_DISABLE_PROBATION_SUCCESSES`] wins → history forgiven;
/// * [`probation_failure_ceiling`] losses → re-quarantined one tier higher (the
///   point at which winning has become arithmetically impossible);
/// * this many dispatches consumed without either → re-quarantined.
///
/// The third clause is what makes the bound unconditional. It is counted in
/// [`HealthTracker::note_dispatch_accepted`], at the moment a task is actually
/// exposed, so it holds even when sessions produce no outcome record at all
/// (killed task, leader failover, pod OOM) — the same fail-closed reasoning as
/// the probe itself. Exposure per released quarantine is therefore at most
/// `1 + HARD_DISABLE_PROBATION_TRIAL_SESSIONS` sessions, by construction,
/// against any input whatsoever.
///
/// `6` leaves a genuinely recovered model room for one bad session out of six
/// (the 4-day outage's model went 10-for-10 and clears on its fifth), while a
/// one-success-in-six model needs at least 5 wins in 6 — probability
/// `C(6,5)·(1/6)^5·(5/6) + (1/6)^6 ≈ 0.00066`, about 1 in 1500 — so it is
/// re-quarantined essentially every cycle, one rung higher each time.
const HARD_DISABLE_PROBATION_TRIAL_SESSIONS: u32 = 6;

const _: () = assert!(
    HARD_DISABLE_PROBATION_TRIAL_SESSIONS >= HARD_DISABLE_PROBATION_SUCCESSES,
    "the probation trial must be long enough that the bucket can actually win it"
);

/// Failures that make the probation trial unwinnable and therefore re-quarantine
/// the bucket at once. Derived, not tuned: once this many sessions are lost,
/// [`HARD_DISABLE_PROBATION_SUCCESSES`] wins can no longer fit inside
/// [`HARD_DISABLE_PROBATION_TRIAL_SESSIONS`].
///
/// This counts EVERY kind of failure record, including a throttle-classified
/// stall (`record_stall(escalate = false)`). That is deliberate, and it is what
/// makes probation bite on the class the 5.75h flap incident actually belonged
/// to: the throttle ladder normally waits for
/// [`PERSISTENT_THROTTLE_TRIP_THRESHOLD`] (6) consecutive trips before it
/// escalates at all, so a bucket on probation would otherwise burn six 429
/// crashes before being re-quarantined. On probation it burns two.
const fn probation_failure_ceiling() -> u32 {
    HARD_DISABLE_PROBATION_TRIAL_SESSIONS - HARD_DISABLE_PROBATION_SUCCESSES + 1
}

/// Wall-clock instant as whole Unix seconds. Pre-epoch times (only reachable
/// from a badly-skewed host clock) render as a negative offset rather than
/// panicking.
fn unix_seconds(wall: SystemTime) -> i64 {
    match wall.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

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
    /// Seconds until the next half-open probe dispatch is admissible. `None`
    /// when the bucket is not hard-disabled; `Some(0)` when a probe is
    /// admissible right now. This is the field that answers "will this recover
    /// on its own, and when?" — the question an operator could not answer for
    /// four days. Purely derived for display; `restore_all` reads the absolute
    /// deadline below instead, so that a snapshot taken long before a boot is
    /// not treated as if it were taken at boot.
    #[serde(default)]
    pub hard_disable_probe_seconds_remaining: Option<u64>,
    /// Absolute **wall-clock** deadline for the next half-open probe, as a Unix
    /// timestamp in seconds. `None` when the bucket is not hard-disabled.
    ///
    /// This is what makes a quarantine survive a restart without being either
    /// wiped or extended. `Instant`s are monotonic and meaningless across a
    /// process boundary, so the first version of this change re-anchored the
    /// deadline at `now + quarantine_for_tier(tier)` on boot — charging a FULL
    /// fresh quarantine every restart. Measured against this repo's real deploy
    /// cadence (median gap 6.08h across the last 25 release tags, 12 of 24 gaps
    /// under 6h, some as low as 0.6h) that starved the probe outright: a deploy
    /// every 4h admitted **zero** probes in 30 days, and the escalation ladder
    /// made it worse, not better — at the 7-day rung essentially every deploy
    /// lands inside the quarantine. That is the 4-day outage reproduced through
    /// a different door.
    #[serde(default)]
    pub hard_disable_probe_at_unix: Option<i64>,
    /// The bucket's quarantine was released by a successful probe but it has not
    /// yet produced enough clean sessions to have its hard-disable history
    /// forgiven: it is dispatchable at full throughput, but a single breaker
    /// trip re-quarantines it one rung higher. `#[serde(default)]` for
    /// back-compat with pre-half-open snapshots.
    #[serde(default)]
    pub hard_disable_on_probation: bool,
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
    /// failure/success that follows.
    ///
    /// This is the **authority gate on quarantine transitions**, and nothing
    /// else may move them:
    ///
    /// * Only an admitted probe's success can release the latch. A success from
    ///   a session that was already in flight when the latch was set proves
    ///   nothing about the quarantine — for a *flapping* model such successes
    ///   are routine, and letting one release the quarantine having admitted
    ///   zero probes hands the lane back for free.
    /// * Only an admitted probe's failure can push the deadline out. Not every
    ///   dispatch path consults the breaker (e.g. evidence-dispatch recovery
    ///   reaches the pool without a `HealthTracker` read at all), so a
    ///   health-blind path failing every few hours would otherwise re-anchor the
    ///   quarantine forever and produce a silent permanent outage — the exact
    ///   bug this whole change exists to remove, reintroduced by a different
    ///   route. Measured: one failure record every 5h admitted **zero** probes
    ///   in 30 simulated days, with the tier pinned at 0 so nothing showed it.
    /// * It also makes the tier escalate once per *probe* rather than once per
    ///   failure record, since a single bad session can emit both a stall and a
    ///   typed failure.
    ///
    /// Note this gate does NOT weaken the fail-closed bound: exposure is bounded
    /// by `note_dispatch_accepted` pushing the deadline at accept time, before
    /// any verdict exists.
    ///
    /// Runtime-only: a restart drops it, which at worst costs one un-escalated
    /// quarantine, and `restore_all` re-anchors the deadline anyway.
    probe_outstanding: bool,
    /// The latch was released by a successful half-open probe, but the bucket has
    /// not yet won its trial (see [`HARD_DISABLE_PROBATION_TRIAL_SESSIONS`]).
    /// While set, the bucket is dispatchable at full throughput but every
    /// dispatch and every outcome is counted against the probation ledger below.
    ///
    /// Persisted, because forgetting it across a restart is what would let an
    /// intermittently-broken model launder its history by waiting for a deploy.
    hard_disable_on_probation: bool,
    /// Probation ledger: sessions won so far in the current trial. Reset by
    /// `release_quarantine_to_probation`, so a success recorded while the bucket
    /// was still QUARANTINED cannot pre-charge the trial — the flaw that let
    /// four stray successes plus one probe success deliver an immediate full
    /// pardon in the same call.
    probation_successes: u32,
    /// Probation ledger: sessions lost so far in the current trial. Counted
    /// independently of `probation_successes` — neither resets the other — which
    /// is what makes the trial immune to failure clustering.
    probation_failures: u32,
    /// Probation ledger: dispatches actually handed out during the current
    /// trial, counted at acceptance. This is the outcome-independent bound: a
    /// session that never reports anything still consumes trial budget, so
    /// exposure is capped whatever the sessions do.
    probation_dispatches: u32,
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

    /// Put a bucket back into quarantine, one rung further up the ladder when it
    /// was already carrying a tier (a probation re-quarantine, or a failed
    /// probe). Shared by every route into the hard-disabled state so they cannot
    /// drift apart.
    fn enter_quarantine(&mut self, now: Instant, escalate_tier: bool) {
        self.hard_disable_probe_tier = if escalate_tier {
            self.hard_disable_probe_tier
                .saturating_add(1)
                .min(HARD_DISABLE_MAX_PROBE_TIER)
        } else {
            0
        };
        self.hard_disable_on_probation = false;
        self.clear_probation_ledger();
        self.hard_disabled = true;
        // No ordinary cooldown: clear the deadline so `is_available` relies
        // solely on the quarantine gate.
        self.cooldown_until = None;
        self.probe_available_at = Some(now + quarantine_for_tier(self.hard_disable_probe_tier));
        self.probe_outstanding = false;
    }

    fn clear_probation_ledger(&mut self) {
        self.probation_successes = 0;
        self.probation_failures = 0;
        self.probation_dispatches = 0;
    }

    /// Record a genuine breaker trip against the rolling trip-rate window and
    /// hard-disable the bucket if the ceiling is reached. Returns `true` when
    /// this trip crossed the ceiling (for one-shot logging).
    ///
    /// Note this is the ORDINARY path only. A bucket on probation is
    /// re-quarantined by its trial ledger (`charge_probation_failure` /
    /// `charge_probation_dispatch`), not by this rolling window: a trip needs
    /// [`CIRCUIT_BREAKER_THRESHOLD`] *consecutive* failures, so keying probation
    /// off it made probation defeated by failure clustering rather than by
    /// failure rate. See [`HARD_DISABLE_PROBATION_TRIAL_SESSIONS`].
    fn register_trip(&mut self, now: Instant) -> bool {
        // Prune trips that have aged out of the window, then record this one.
        self.trip_times
            .retain(|t| now.duration_since(*t) < TRIP_RATE_WINDOW);
        self.trip_times.push(now);
        if !self.hard_disabled && self.trip_times.len() >= TRIP_RATE_CEILING {
            self.enter_quarantine(now, false);
            return true;
        }
        false
    }

    /// Charge one lost session against the probation trial, re-quarantining the
    /// bucket when the trial has become unwinnable. Returns `true` if it did.
    ///
    /// Counted, never streaked, and counted for EVERY class of failure record —
    /// including throttle-classified stalls, which the ordinary ladder would
    /// otherwise let run to `PERSISTENT_THROTTLE_TRIP_THRESHOLD` (6) before
    /// escalating at all.
    fn charge_probation_failure(&mut self, now: Instant) -> bool {
        if !self.hard_disable_on_probation {
            return false;
        }
        self.probation_failures = self.probation_failures.saturating_add(1);
        if self.probation_failures >= probation_failure_ceiling() {
            self.enter_quarantine(now, true);
            return true;
        }
        false
    }

    /// Charge one won session against the probation trial, forgiving the
    /// hard-disable history once the bucket has won enough of them. Returns
    /// `true` if probation ended.
    fn charge_probation_success(&mut self) -> bool {
        if !self.hard_disable_on_probation {
            return false;
        }
        self.probation_successes = self.probation_successes.saturating_add(1);
        if self.probation_successes >= HARD_DISABLE_PROBATION_SUCCESSES {
            self.clear_hard_disable();
            return true;
        }
        false
    }

    /// Charge one dispatch against the probation trial and re-quarantine the
    /// bucket if the trial budget is now spent.
    ///
    /// This is the clause that makes the probation bound unconditional: it fires
    /// on exposure rather than on outcome, so a run of sessions that never
    /// report anything (killed task, leader failover, pod OOM) still ends the
    /// trial instead of leaving the lane open indefinitely.
    fn charge_probation_dispatch(&mut self, now: Instant) -> bool {
        if !self.hard_disable_on_probation {
            return false;
        }
        self.probation_dispatches = self.probation_dispatches.saturating_add(1);
        if self.probation_dispatches >= HARD_DISABLE_PROBATION_TRIAL_SESSIONS
            && self.probation_successes < HARD_DISABLE_PROBATION_SUCCESSES
        {
            self.enter_quarantine(now, true);
            return true;
        }
        false
    }

    /// Resolve an **outstanding** half-open probe as failed: re-latch, push the
    /// deadline a full quarantine out, and — unless the failure was a transient
    /// upstream fault — advance the escalation tier.
    ///
    /// Does nothing when no probe is outstanding, and that guard is load-bearing
    /// (see `probe_outstanding`): not every dispatch path consults the breaker,
    /// so an unconditional re-anchor lets a health-blind path postpone the probe
    /// forever and turn the quarantine into the silent permanent outage this
    /// whole change exists to remove. Returns `true` when a probe was actually
    /// resolved, so callers log a verdict rather than a phantom one.
    ///
    /// Escalation is also charged once per *probe* rather than once per failure
    /// record, because a single doomed session can emit both a stall and a typed
    /// failure and double-charging would jump a model several rungs on one
    /// dispatch.
    fn resolve_probe_failure(&mut self, now: Instant, escalate: bool) -> bool {
        if !self.probe_outstanding {
            return false;
        }
        self.probe_outstanding = false;
        if escalate {
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
        true
    }

    /// Release the latch on a successful half-open probe, moving the bucket onto
    /// **probation** rather than back to a clean slate.
    ///
    /// The distinction is the whole point. A probe success is real evidence the
    /// model works right now, so the lane must reopen at full throughput — that
    /// is what fixes the 4-day outage. But it is evidence from ONE session about
    /// a bucket that reached eight trips in six hours, and treating it as a full
    /// pardon (clearing the tier, the trip window and the escalating cooldown)
    /// is what let a one-success-in-six model escape the ladder entirely. So the
    /// tier survives, and the bucket must win
    /// [`HARD_DISABLE_PROBATION_SUCCESSES`] of at most
    /// [`HARD_DISABLE_PROBATION_TRIAL_SESSIONS`] sessions before the history is
    /// written off.
    ///
    /// The ledger is reset here, and that reset is load-bearing: successes
    /// recorded while the bucket was still QUARANTINED must not pre-charge the
    /// trial. Without it, four stray successes on a quarantined bucket (a
    /// health-blind path, or sessions in flight at latch time — the class the
    /// `record_success` gate itself calls routine) left the counter at four, so
    /// the very next probe success released the latch and fell straight through
    /// to the exit check in the same call: a full pardon, byte-identical to a
    /// never-tripped model, from one probe.
    fn release_quarantine_to_probation(&mut self) {
        self.hard_disabled = false;
        self.probe_available_at = None;
        self.probe_outstanding = false;
        self.hard_disable_on_probation = true;
        self.clear_probation_ledger();
        // `hard_disable_probe_tier` is deliberately PRESERVED.
        self.auto_disabled = false;
        self.cooldown_until = None;
    }

    /// Fully clear the hard-disable, its probation and its escalation ladder.
    /// This is the human `enable`/`reset` authority path, and the probation exit
    /// once a bucket has re-proven itself. Also clears the ordinary
    /// auto-disable, because a quarantined bucket carries `auto_disabled: true`
    /// with `cooldown_until: None` — a state `is_available` would otherwise read
    /// as a never-expiring cooldown.
    fn clear_hard_disable(&mut self) {
        self.hard_disabled = false;
        self.probe_available_at = None;
        self.hard_disable_probe_tier = 0;
        self.probe_outstanding = false;
        self.hard_disable_on_probation = false;
        self.clear_probation_ledger();
        self.auto_disabled = false;
        self.cooldown_until = None;
    }

    /// Seconds until this bucket's next half-open probe is admissible; `None`
    /// when it is not quarantined, and `Some(0)` when one is admissible now.
    fn probe_seconds_remaining(&self, now: Instant) -> Option<u64> {
        if !self.hard_disabled {
            return None;
        }
        let at = self.probe_available_at?;
        Some(at.saturating_duration_since(now).as_secs())
    }

    fn cooldown_seconds_remaining(&self, now: Instant) -> Option<u64> {
        let until = self.cooldown_until?;
        if until > now {
            Some((until - now).as_secs())
        } else {
            None
        }
    }

    fn to_health(&self, key: &HealthKey, now: Instant, wall_now: SystemTime) -> ModelHealth {
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
            hard_disable_probe_seconds_remaining: self.probe_seconds_remaining(now),
            hard_disable_probe_at_unix: self
                .probe_seconds_remaining(now)
                .map(|remaining| unix_seconds(wall_now).saturating_add(remaining as i64)),
            hard_disable_on_probation: self.hard_disable_on_probation,
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

        // Half-open probe verdict — see `resolve_probe_failure`. It re-latches
        // and escalates only when a probe is actually outstanding; a failure
        // arriving from a health-blind dispatch path leaves the quarantine
        // deadline alone rather than postponing the probe forever. Either way
        // we return: falling through to the ordinary ladder while latched would
        // need three strikes to react and would log a phantom second trip.
        if state.hard_disabled {
            if state.resolve_probe_failure(now, true) {
                tracing::warn!(
                    model_id = %key.model_id,
                    scope = ?key.scope,
                    probe_tier = state.hard_disable_probe_tier,
                    next_probe_in_secs =
                        quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                    "half-open probe of a hard-disabled model failed — re-quarantined"
                );
            }
            return;
        }

        // Probation trial: one session lost. Charged for every failure class —
        // including throttle-classified stalls, which the ordinary ladder would
        // let run to PERSISTENT_THROTTLE_TRIP_THRESHOLD before escalating at all.
        if state.charge_probation_failure(now) {
            tracing::warn!(
                model_id = %key.model_id,
                scope = ?key.scope,
                probation_failures = probation_failure_ceiling(),
                probe_tier = state.hard_disable_probe_tier,
                next_probe_in_secs = quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                "model lost its probation trial — re-quarantined one tier higher"
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
        // NOTE: nothing probation-related is charged before the quarantine gate
        // below. The probation ledger is advanced only by
        // `charge_probation_success`, which no-ops unless the bucket is actually
        // on probation, so a success on a QUARANTINED bucket cannot pre-charge
        // the trial it will later be handed.

        if state.hard_disabled {
            // Only an ADMITTED PROBE's success may release a quarantine. A
            // success from a session that was already in flight when the latch
            // was set (realistic whenever `max_sessions > 1` for the same
            // `(scope, model)`) says nothing about whether the quarantine was
            // right: for a *flapping* model such successes are routine, and
            // releasing on one hands the lane back having admitted zero probes.
            // Everything the quarantine owns — the latch, the deadline, the
            // tier — is therefore left untouched here, as are the ladder
            // counters below, which would otherwise let a stray success wipe
            // the history that justified the latch.
            if !state.probe_outstanding {
                return;
            }
            // The probe came back clean. Reopen the lane at full throughput,
            // but onto PROBATION rather than a clean slate — see
            // `release_quarantine_to_probation`.
            state.release_quarantine_to_probation();
            tracing::info!(
                model_id = %model_id,
                scope = ?scope,
                probe_tier = state.hard_disable_probe_tier,
                probation_successes_required = HARD_DISABLE_PROBATION_SUCCESSES,
                probation_trial_sessions = HARD_DISABLE_PROBATION_TRIAL_SESSIONS,
                "half-open probe succeeded — quarantine released onto probation \
                 (must win {HARD_DISABLE_PROBATION_SUCCESSES} of the next \
                 {HARD_DISABLE_PROBATION_TRIAL_SESSIONS} sessions)"
            );
            // The probe's own success is NOT charged to the trial: the ledger was
            // just reset, and the trial is about what the model does with the
            // lane it has been handed back.
            return;
        }

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

        // Probation trial: one session won. Counted, not streaked — an
        // interleaved failure does not reset it, and does not need to, because
        // the failure is counted independently against
        // `probation_failure_ceiling()`.
        if state.charge_probation_success() {
            tracing::info!(
                model_id = %model_id,
                scope = ?scope,
                required = HARD_DISABLE_PROBATION_SUCCESSES,
                "model won its probation trial after a released quarantine — \
                 hard-disable history forgiven"
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
    /// future before the probe's verdict is known. For one on **probation** it
    /// spends a session of the trial budget, ending the trial when the budget
    /// runs out.
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
            // Probation trial budget. Charged on EXPOSURE rather than on
            // outcome, so a run of sessions that never report anything cannot
            // hold the lane open: the trial ends either way.
            if state.charge_probation_dispatch(now) {
                tracing::warn!(
                    model_id = %model_id,
                    scope = ?scope,
                    trial_sessions = HARD_DISABLE_PROBATION_TRIAL_SESSIONS,
                    probe_tier = state.hard_disable_probe_tier,
                    next_probe_in_secs =
                        quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                    "model spent its probation trial budget without winning it — \
                     re-quarantined one tier higher"
                );
            }
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
            if state.resolve_probe_failure(now, true) {
                tracing::warn!(
                    model_id = %key.model_id,
                    scope = ?key.scope,
                    probe_tier = state.hard_disable_probe_tier,
                    next_probe_in_secs =
                        quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                    "half-open probe of a hard-disabled model failed — re-quarantined"
                );
            }
            return;
        }

        // Probation trial: one session lost. Charged for every failure class —
        // including throttle-classified stalls, which the ordinary ladder would
        // let run to PERSISTENT_THROTTLE_TRIP_THRESHOLD before escalating at all.
        if state.charge_probation_failure(now) {
            tracing::warn!(
                model_id = %key.model_id,
                scope = ?key.scope,
                probation_failures = probation_failure_ceiling(),
                probe_tier = state.hard_disable_probe_tier,
                next_probe_in_secs = quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                "model lost its probation trial — re-quarantined one tier higher"
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

        // Half-open probe verdict — see `apply_breaker_check_for`, but WITHOUT
        // escalation (`escalate = false`). The probe produced nothing usable, so
        // the bucket serves another quarantine period; it does not serve a
        // longer one. This crate's own position is that a transient upstream
        // fault is a load signal and says nothing about model health — it trips
        // at `TRANSIENT_BREAKER_THRESHOLD` (20) rather than
        // `CIRCUIT_BREAKER_THRESHOLD` (3) for exactly that reason — and the
        // 4-day outage's latch was itself minted during a transient provider
        // incident. Letting one 5xx double a quarantine would compound that
        // mistake at the one moment the model is being re-examined.
        if state.hard_disabled {
            if state.resolve_probe_failure(now, false) {
                tracing::warn!(
                    model_id = %key.model_id,
                    scope = ?key.scope,
                    probe_tier = state.hard_disable_probe_tier,
                    next_probe_in_secs =
                        quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                    "half-open probe of a hard-disabled model hit a TRANSIENT upstream fault \
                     — re-quarantined at the same tier, not escalated"
                );
            }
            return;
        }

        // Probation trial: one session lost. Charged for every failure class —
        // including throttle-classified stalls, which the ordinary ladder would
        // let run to PERSISTENT_THROTTLE_TRIP_THRESHOLD before escalating at all.
        if state.charge_probation_failure(now) {
            tracing::warn!(
                model_id = %key.model_id,
                scope = ?key.scope,
                probation_failures = probation_failure_ceiling(),
                probe_tier = state.hard_disable_probe_tier,
                next_probe_in_secs = quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                "model lost its probation trial — re-quarantined one tier higher"
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
            if state.resolve_probe_failure(now, true) {
                tracing::warn!(
                    model_id = %key.model_id,
                    scope = ?key.scope,
                    probe_tier = state.hard_disable_probe_tier,
                    next_probe_in_secs =
                        quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                    "half-open probe of a hard-disabled model stalled — re-quarantined"
                );
            }
            return;
        }

        // Probation trial: one session lost. Charged for every failure class —
        // including throttle-classified stalls, which the ordinary ladder would
        // let run to PERSISTENT_THROTTLE_TRIP_THRESHOLD before escalating at all.
        if state.charge_probation_failure(now) {
            tracing::warn!(
                model_id = %key.model_id,
                scope = ?key.scope,
                probation_failures = probation_failure_ceiling(),
                probe_tier = state.hard_disable_probe_tier,
                next_probe_in_secs = quarantine_for_tier(state.hard_disable_probe_tier).as_secs(),
                "model lost its probation trial — re-quarantined one tier higher"
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
        let wall_now = self.clock.now();
        let mut health: Vec<_> = map
            .iter()
            .map(|(key, s)| s.to_health(key, now, wall_now))
            .collect();
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
        let wall_now = self.clock.now();
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
                state.hard_disable_probe_tier = health
                    .hard_disable_probe_tier
                    .min(HARD_DISABLE_MAX_PROBE_TIER);
                // `Instant`s are monotonic and meaningless across a process
                // boundary, so the quarantine's remaining time is carried as an
                // absolute WALL-CLOCK deadline and converted back to a local
                // deadline here. A restart therefore neither wipes the
                // quarantine nor extends it: elapsed downtime counts against it
                // exactly as uptime would.
                //
                // The alternative — re-anchoring at `now + quarantine_for_tier`
                // — looked conservative and was not. It charged a FULL fresh
                // quarantine on every boot, so against this repo's real deploy
                // cadence (median gap 6.08h over the last 25 release tags, 12 of
                // 24 gaps under 6h) the probe simply never fired: a deploy every
                // 4h admitted ZERO probes in 30 days. The escalation ladder made
                // it strictly worse, because at the 7-day rung essentially every
                // deploy lands inside the quarantine. That is the 4-day outage
                // reproduced through a different door, which is why the deadline
                // is now persisted rather than recomputed.
                //
                // The remaining time is still clamped to this tier's quarantine:
                // a corrupt or clock-skewed snapshot may shorten a quarantine
                // (bounded by the ordinary ladder) but can never manufacture one
                // longer than the tier allows.
                let remaining = match health.hard_disable_probe_at_unix {
                    Some(deadline) => Duration::from_secs(
                        deadline.saturating_sub(unix_seconds(wall_now)).max(0) as u64,
                    )
                    .min(quarantine_for_tier(state.hard_disable_probe_tier)),
                    // Pre-deadline snapshot (or a bucket persisted by an older
                    // build): fall back to a full quarantine from now. Strictly
                    // the conservative direction, and it self-corrects on the
                    // next persist.
                    None => quarantine_for_tier(state.hard_disable_probe_tier),
                };
                state.probe_available_at = Some(now + remaining);
            } else if health.hard_disable_on_probation {
                // Released by a probe but not yet re-proven. Probation is
                // persisted for the same reason the tier is: without it, a
                // model whose quarantine keeps getting released by lucky probes
                // could launder its history through a deploy and go back to
                // needing eight fresh trips. The trial LEDGER is runtime-only,
                // so a restart restarts the trial from zero: the bucket must
                // win its sessions again. That direction is safe — it can only
                // keep a bucket on probation longer — and the trial is bounded
                // by `HARD_DISABLE_PROBATION_TRIAL_SESSIONS` dispatches either
                // way, so restarting it cannot become an exposure leak.
                state.hard_disable_on_probation = true;
                // The tier the bucket carried out of its last quarantine, so
                // the re-latch a single trip triggers resumes the ladder one
                // rung higher rather than restarting at the 6h base.
                state.hard_disable_probe_tier = health
                    .hard_disable_probe_tier
                    .min(HARD_DISABLE_MAX_PROBE_TIER);
                if health.auto_disabled {
                    if let Some(seconds) = health.cooldown_seconds_remaining.filter(|s| *s > 0) {
                        state.cooldown_until = Some(now + Duration::from_secs(seconds));
                    } else {
                        state.auto_disabled = false;
                    }
                }
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
        let wall_now = self.clock.now();
        let key = HealthKey::new(scope, model_id);
        let map = self.inner.lock().unwrap();
        map.get(&key)
            .map(|s| s.to_health(&key, now, wall_now))
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
                hard_disable_probe_seconds_remaining: None,
                hard_disable_probe_at_unix: None,
                hard_disable_on_probation: false,
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
