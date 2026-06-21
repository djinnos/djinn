// Touch to advance main HEAD and trigger a warm job (verification warm-base
// cargo cache validation, 2026-06-16). No behavior change.
use std::sync::OnceLock;

use metrics_exporter_prometheus::{BuildError, PrometheusBuilder, PrometheusHandle};

pub const PROMETHEUS_TEXT_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

const DISPATCH_ATTEMPTS_TOTAL: &str = "djinn_dispatch_attempts_total";
const DISPATCH_LAST_SUCCESS_TIMESTAMP: &str = "djinn_dispatch_last_success_timestamp";
const DISPATCH_COOLDOWNS_ACTIVE: &str = "djinn_dispatch_cooldowns_active";
const INFLIGHT_LEDGER_SIZE: &str = "djinn_inflight_ledger_size";
const USER_CAP_UTILIZATION: &str = "djinn_user_cap_utilization";
const SLOT_POOL: &str = "djinn_slot_pool";
const DISPATCH_OUTCOMES: [&str; 5] = ["ok", "cooldown", "cap", "breaker", "error"];
const BREAKER_TRIPS_TOTAL: &str = "djinn_breaker_trips_total";
const BREAKER_STATE: &str = "djinn_breaker_state";
const ZOMBIE_REAPS_TOTAL: &str = "djinn_zombie_reaps_total";
const ZOMBIE_REAP_KINDS: [&str; 3] = ["startup", "periodic", "stall"];
const LEAD_ESCALATIONS_TOTAL: &str = "djinn_lead_escalations_total";
const TASK_REOPENS_TOTAL: &str = "djinn_task_reopens_total";
const TASKS_PARKED_TOTAL: &str = "djinn_tasks_parked_total";
const PR_POLLER_TRACKED: &str = "djinn_pr_poller_tracked";
const MERGE_FAILURES_TOTAL: &str = "djinn_merge_failures_total";
const DOCTOR_FINDINGS: &str = "djinn_doctor_findings";
const DOCTOR_RUN_DURATION_SECONDS: &str = "djinn_doctor_run_duration_seconds";
const CARGO_TARGET_SEED_TOTAL: &str = "djinn_cargo_target_seed_total";
const CARGO_SEED_HIT_TOTAL: &str = "djinn_cargo_seed_hit_total";
const CARGO_SEED_COLD_TOTAL: &str = "djinn_cargo_seed_cold_total";
const CARGO_WARM_BASE_FRESHNESS_SECONDS: &str = "djinn_cargo_warm_base_freshness_seconds";
const CARGO_WARM_STEP_TOTAL: &str = "djinn_cargo_warm_step_total";
const CARGO_WARM_STEP_OUTCOMES: [&str; 3] = ["ok", "failed", "spawn_error"];
const CARGO_WARM_STEP_WORKSPACE_PATH_HASH: &str = "djinn_cargo_warm_step_workspace_path_hash";
const CARGO_WARM_STEP_FRESH_COUNT: &str = "djinn_cargo_warm_step_fresh_count";
const CARGO_WARM_STEP_COMPILING_COUNT: &str = "djinn_cargo_warm_step_compiling_count";
const SLOT_POOL_STATES: [&str; 2] = ["free", "busy"];
const JIT_PITFALL_HINTS_TOTAL: &str = "djinn_jit_pitfall_hints_total";
const JIT_PITFALL_OUTCOMES: [&str; 7] = [
    "disabled_default_off",
    "disabled_kill_switch",
    "non_first_modification",
    "eligible_search",
    "injected",
    "empty",
    "error",
];

// ─── Stale-PR/branch reconciliation sweep ────────────────────────────────
const STALE_PR_REAPED_TOTAL: &str = "djinn_stale_pr_reaped_total";
const STALE_BRANCH_REAPED_TOTAL: &str = "djinn_stale_branch_reaped_total";
const STALE_PR_SKIPPED_TOTAL: &str = "djinn_stale_pr_skipped_total";

static HANDLE: OnceLock<Result<PrometheusHandle, String>> = OnceLock::new();

/// Install the process-global Prometheus recorder.
///
/// Initialization is synchronous and idempotent. The first caller installs the
/// global `metrics` recorder; later callers observe the same result without
/// taking application locks or requiring an async runtime.
pub fn init() -> Result<(), String> {
    handle().map(|_| ())
}

pub mod breaker {
    /// Increment the breaker-trip counter. Synchronous and non-async by design.
    pub fn increment_trip() {
        metrics::counter!(super::BREAKER_TRIPS_TOTAL).increment(1);
    }

    /// Set the scrape-time breaker-state gauge for a `(scope, model)` bucket.
    pub fn set_state(scope: &str, model: &str, value: f64) {
        metrics::gauge!(super::BREAKER_STATE, "scope" => scope.to_owned(), "model" => model.to_owned()).set(value);
    }
}

pub mod zombie {
    pub const KIND_STARTUP: &str = "startup";
    pub const KIND_PERIODIC: &str = "periodic";
    pub const KIND_STALL: &str = "stall";

    /// Increment the zombie-reap counter for one of the stable kind labels.
    pub fn increment_reap(kind: &'static str) {
        metrics::counter!(super::ZOMBIE_REAPS_TOTAL, "kind" => kind).increment(1);
    }
}

pub mod lead {
    /// Increment the Lead-escalation counter. Synchronous and non-async by design.
    pub fn increment_escalation() {
        metrics::counter!(super::LEAD_ESCALATIONS_TOTAL).increment(1);
    }
}

pub mod jit_pitfalls {
    pub const OUTCOME_DISABLED_DEFAULT_OFF: &str = "disabled_default_off";
    pub const OUTCOME_DISABLED_KILL_SWITCH: &str = "disabled_kill_switch";
    pub const OUTCOME_NON_FIRST_MODIFICATION: &str = "non_first_modification";
    pub const OUTCOME_ELIGIBLE_SEARCH: &str = "eligible_search";
    pub const OUTCOME_INJECTED: &str = "injected";
    pub const OUTCOME_EMPTY: &str = "empty";
    pub const OUTCOME_ERROR: &str = "error";

    /// Increment the JIT-pitfall hint counter for one stable outcome label.
    ///
    /// This intentionally accepts only `'static` labels so hot-path callers keep
    /// metric cardinality bounded. Rich metadata belongs in structured tracing
    /// fields emitted next to this counter.
    pub fn increment_outcome(outcome: &'static str) {
        metrics::counter!(super::JIT_PITFALL_HINTS_TOTAL, "outcome" => outcome).increment(1);
    }
}

pub mod task {
    /// Increment the task-reopen counter when a transition successfully bumps `reopen_count`.
    pub fn increment_reopen() {
        metrics::counter!(super::TASK_REOPENS_TOTAL).increment(1);
    }

    /// Increment the parked-task counter when the coordinator records a terminal task park.
    pub fn increment_parked() {
        metrics::counter!(super::TASKS_PARKED_TOTAL).increment(1);
    }
}

pub mod pr_poller {
    /// Set the O(1)-cardinality tracked fast-path PR count.
    pub fn set_tracked(count: usize) {
        metrics::gauge!(super::PR_POLLER_TRACKED).set(count as f64);
    }

    /// Increment merge failures that fall through to PR-poller reopen handling.
    pub fn increment_merge_failure() {
        metrics::counter!(super::MERGE_FAILURES_TOTAL).increment(1);
    }
}

pub mod doctor {
    pub const FINDINGS: &str = super::DOCTOR_FINDINGS;
    pub const RUN_DURATION_SECONDS: &str = super::DOCTOR_RUN_DURATION_SECONDS;

    /// Set the number of findings emitted by one stable doctor check name.
    ///
    /// The only metric label is `check`, and callers should pass
    /// `DoctorCheck::name()` directly to keep cardinality bounded.
    pub fn set_findings(check: &str, count: usize) {
        metrics::gauge!(super::DOCTOR_FINDINGS, "check" => check.to_owned()).set(count as f64);
    }

    /// Record the last run duration for one stable doctor check name.
    ///
    /// This gauge keeps the rendered sample exactly
    /// `djinn_doctor_run_duration_seconds{check="..."}` with no severity,
    /// entity, bucket, or free-form labels.
    pub fn set_run_duration_seconds(check: &str, seconds: f64) {
        metrics::gauge!(super::DOCTOR_RUN_DURATION_SECONDS, "check" => check.to_owned())
            .set(seconds);
    }

    /// Convenience wrapper for callers measuring with `std::time::Duration`.
    pub fn record_run_duration(check: &str, duration: std::time::Duration) {
        set_run_duration_seconds(check, duration.as_secs_f64());
    }
}

pub mod cargo_cache {
    pub const SEED_HIT_TOTAL: &str = super::CARGO_SEED_HIT_TOTAL;
    pub const SEED_COLD_TOTAL: &str = super::CARGO_SEED_COLD_TOTAL;
    pub const WARM_BASE_FRESHNESS_SECONDS: &str = super::CARGO_WARM_BASE_FRESHNESS_SECONDS;
    pub const WARM_STEP_FRESH_COUNT: &str = super::CARGO_WARM_STEP_FRESH_COUNT;
    pub const WARM_STEP_COMPILING_COUNT: &str = super::CARGO_WARM_STEP_COMPILING_COUNT;

    /// Increment the Cargo target warm-base seed hit counter for a project.
    pub fn record_seed_hit(project_id: &str) {
        metrics::counter!(super::CARGO_SEED_HIT_TOTAL, "project_id" => project_id.to_owned())
            .increment(1);
    }

    /// Increment the Cargo target cold-start fallback counter for a project and reason.
    pub fn record_seed_cold(project_id: &str, fallback_reason: &str) {
        metrics::counter!(
            super::CARGO_SEED_COLD_TOTAL,
            "project_id" => project_id.to_owned(),
            "fallback_reason" => fallback_reason.to_owned()
        )
        .increment(1);
    }

    /// Set the elapsed age/freshness gauge for a just-produced warm Cargo base.
    pub fn record_warm_base_freshness(project_id: &str, age_secs: f64) {
        metrics::gauge!(
            super::CARGO_WARM_BASE_FRESHNESS_SECONDS,
            "project_id" => project_id.to_owned()
        )
        .set(age_secs);
    }

    /// Set the number of Cargo units reported as `Fresh` for one warm step.
    pub fn record_warm_step_fresh_count(project_id: &str, step: &str, count: usize) {
        metrics::gauge!(
            super::CARGO_WARM_STEP_FRESH_COUNT,
            "project_id" => project_id.to_owned(),
            "step" => step.to_owned()
        )
        .set(count as f64);
    }

    /// Set the number of Cargo units reported as `Compiling` for one warm step.
    pub fn record_warm_step_compiling_count(project_id: &str, step: &str, count: usize) {
        metrics::gauge!(
            super::CARGO_WARM_STEP_COMPILING_COUNT,
            "project_id" => project_id.to_owned(),
            "step" => step.to_owned()
        )
        .set(count as f64);
    }
}

pub mod stale_sweep {
    /// Reason labels for the `djinn_stale_pr_skipped_total` counter.
    pub const REASON_GRACE_PERIOD: &str = "grace_period";
    pub const REASON_NOT_BOT: &str = "not_bot";
    pub const REASON_IN_MERGE_QUEUE: &str = "in_merge_queue";
    pub const REASON_ENABLED_FALSE: &str = "disabled";
    pub const REASON_TASK_OPEN: &str = "task_open";
    pub const REASON_PR_MERGED: &str = "pr_merged";
    pub const REASON_NO_INSTALLATION: &str = "no_installation";
    pub const REASON_API_ERROR: &str = "api_error";

    /// Increment the stale-PR reaped counter (a PR was closed by the sweep).
    pub fn increment_pr_reaped() {
        metrics::counter!(super::STALE_PR_REAPED_TOTAL).increment(1);
    }

    /// Increment the stale-branch reaped counter (a remote branch was deleted by the sweep).
    pub fn increment_branch_reaped() {
        metrics::counter!(super::STALE_BRANCH_REAPED_TOTAL).increment(1);
    }

    /// Increment the stale-PR skipped counter with a reason label.
    pub fn increment_pr_skipped(reason: &'static str) {
        metrics::counter!(super::STALE_PR_SKIPPED_TOTAL, "reason" => reason).increment(1);
    }
}

pub mod cargo_warm_step {
    pub const OUTCOME_OK: &str = "ok";
    pub const OUTCOME_FAILED: &str = "failed";
    pub const OUTCOME_SPAWN_ERROR: &str = "spawn_error";

    /// Stable labels for the cargo warm step. Keep this list closed so the
    /// `step` label cardinality stays bounded.
    pub const STEP_CLIPPY: &str = "clippy";
    pub const STEP_CLIPPY_DEFAULT_FEATURES: &str = "clippy_default";
    pub const STEP_BUILD_FALLBACK: &str = "build_fallback";
    pub const STEP_TEST_NO_RUN: &str = "test_no_run";

    pub const STEP_TOTAL: &str = super::CARGO_WARM_STEP_TOTAL;
    pub const WORKSPACE_PATH_HASH: &str = super::CARGO_WARM_STEP_WORKSPACE_PATH_HASH;

    /// Increment the cargo warm-step counter for a `(project_id, step, outcome)`
    /// bucket. The free-form cargo argv is intentionally NOT a label so metric
    /// cardinality stays bounded; the exact argv is logged via structured tracing
    /// instead (see `cargo_metrics` in the worker crate).
    ///
    /// `step` should be one of the stable `STEP_*` constants below so the label
    /// space is closed.
    pub fn increment_step(project_id: &str, step: &'static str, outcome: &'static str) {
        metrics::counter!(
            super::CARGO_WARM_STEP_TOTAL,
            "project_id" => project_id.to_owned(),
            "step" => step,
            "outcome" => outcome,
        )
        .increment(1);
    }

    /// Record a stable, low-cardinality fingerprint of the absolute workspace
    /// directory the worker resolved for the most recent cargo warm step.
    ///
    /// Prometheus labels can't safely carry free-form absolute paths (high
    /// cardinality, possibly sensitive). We hash the path and store the
    /// 64-bit FNV-1a hash as the gauge value — paired with the matching
    /// structured tracing event the worker emits alongside (which DOES carry
    /// the absolute path) the coordinator health sweep can correlate the
    /// hash with the path without exploding label cardinality.
    ///
    /// `project_id` is the only label so cardinality stays bounded by the
    /// number of projects the worker is servicing, not the number of
    /// distinct filesystem layouts.
    pub fn set_workspace_path(project_id: &str, workspace_path: &str) {
        let hash = fnv1a64(workspace_path.as_bytes());
        metrics::gauge!(
            super::CARGO_WARM_STEP_WORKSPACE_PATH_HASH,
            "project_id" => project_id.to_owned()
        )
        .set(hash);
    }

    /// 64-bit FNV-1a hash. Tiny, no extra deps, stable across processes. Good
    /// enough for correlating a workspace path to its health-sweep sample
    /// without exposing the path as a label.
    fn fnv1a64(bytes: &[u8]) -> f64 {
        const OFFSET: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x100000001b3;
        let mut hash = OFFSET;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        hash as f64
    }

    /// Exposed for the public surface so callers can hash a workspace path the
    /// same way `set_workspace_path` does, when they need to match a tracing
    /// event to a metric sample.
    pub fn workspace_path_hash(workspace_path: &str) -> u64 {
        const OFFSET: u64 = 0xcbf29ce484222325;
        const PRIME: u64 = 0x100000001b3;
        let mut hash = OFFSET;
        for byte in workspace_path.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(PRIME);
        }
        hash
    }
}

pub mod cargo_target_seed {
    pub const FALLBACK_REASON_BASE_MISSING: &str = "base_missing";
    pub const FALLBACK_REASON_BASE_NOT_DIRECTORY: &str = "base_not_directory";
    pub const FALLBACK_REASON_BASE_UNUSABLE: &str = "base_unusable";
    pub const FALLBACK_REASON_SCAN_FAILED: &str = "scan_failed";
    pub const FALLBACK_REASON_CLONE_FAILED: &str = "clone_failed";
    pub const FALLBACK_REASON_UNKNOWN: &str = "unknown";

    const OUTCOME_HIT: &str = "hit";
    const OUTCOME_FALLBACK: &str = "fallback";

    /// Increment the Cargo target seed counter for a warm-base hit.
    pub fn increment_seed_hit() {
        metrics::counter!(
            super::CARGO_TARGET_SEED_TOTAL,
            "outcome" => OUTCOME_HIT,
            "fallback_reason" => ""
        )
        .increment(1);
    }

    /// Increment the Cargo target seed counter for a cold fallback reason.
    ///
    /// Callers should pass one of the `FALLBACK_REASON_*` constants. Unexpected
    /// local failures should be mapped to `FALLBACK_REASON_UNKNOWN` rather than
    /// passed through as free-form labels.
    pub fn increment_seed_fallback(reason: &'static str) {
        metrics::counter!(
            super::CARGO_TARGET_SEED_TOTAL,
            "outcome" => OUTCOME_FALLBACK,
            "fallback_reason" => reason
        )
        .increment(1);
    }
}

/// Render the current registry in Prometheus text format.
///
/// Calling this before `init()` is supported: it initializes the recorder first
/// so tests can exercise the render path directly.
pub fn render() -> Result<String, String> {
    handle().map(|handle| prioritize_dispatch_attempts(handle.render()))
}

fn prioritize_dispatch_attempts(rendered: String) -> String {
    const DISPATCH_HELP: &str = "# HELP djinn_dispatch_attempts_total";
    if rendered.starts_with(DISPATCH_HELP) {
        return rendered;
    }

    let trimmed = rendered.trim_end_matches('\n');
    let mut blocks: Vec<&str> = trimmed.split("\n\n").collect();
    if let Some(index) = blocks
        .iter()
        .position(|block| block.starts_with(DISPATCH_HELP))
    {
        let dispatch = blocks.remove(index);
        blocks.insert(0, dispatch);
        let mut reordered = blocks.join("\n\n");
        reordered.push_str("\n\n");
        reordered
    } else {
        rendered
    }
}

fn handle() -> Result<&'static PrometheusHandle, String> {
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .map_err(format_build_error)
                .inspect(|_| register_metrics())
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn format_build_error(error: BuildError) -> String {
    format!("failed to install Prometheus metrics recorder: {error}")
}

fn register_metrics() {
    metrics::describe_counter!(
        DISPATCH_ATTEMPTS_TOTAL,
        "Dispatch attempts partitioned by terminal dispatch outcome."
    );
    for outcome in DISPATCH_OUTCOMES {
        metrics::counter!(DISPATCH_ATTEMPTS_TOTAL, "outcome" => outcome).absolute(0);
    }
    metrics::describe_gauge!(
        DISPATCH_LAST_SUCCESS_TIMESTAMP,
        "Unix timestamp in seconds for the last successful dispatch."
    );
    metrics::describe_gauge!(
        DISPATCH_COOLDOWNS_ACTIVE,
        "Current number of active dispatch cooldown entries."
    );
    metrics::describe_gauge!(
        INFLIGHT_LEDGER_SIZE,
        "Current number of entries in the coordinator in-flight dispatch ledger."
    );
    metrics::describe_gauge!(
        USER_CAP_UTILIZATION,
        "Per user/model dispatch cap utilization ratio: (db_running plus in-flight ledger overlay) divided by configured cap."
    );
    metrics::describe_gauge!(
        SLOT_POOL,
        "Slot pool slots aggregated by state and model. Labels are state=free|busy and model only."
    );
    metrics::describe_counter!(
        BREAKER_TRIPS_TOTAL,
        "Circuit-breaker trips at the authoritative closed-to-open transition."
    );
    metrics::counter!(BREAKER_TRIPS_TOTAL).absolute(0);
    metrics::describe_gauge!(
        BREAKER_STATE,
        "Circuit-breaker state by scope and model: Closed=0.0, HalfOpen=0.5, Open=1.0."
    );
    metrics::describe_counter!(
        ZOMBIE_REAPS_TOTAL,
        "Zombie reaps partitioned by reaper kind."
    );
    for kind in ZOMBIE_REAP_KINDS {
        metrics::counter!(ZOMBIE_REAPS_TOTAL, "kind" => kind).absolute(0);
    }
    metrics::describe_counter!(
        LEAD_ESCALATIONS_TOTAL,
        "Lead escalation requests recorded by the coordinator."
    );
    metrics::counter!(LEAD_ESCALATIONS_TOTAL).absolute(0);
    metrics::describe_counter!(TASK_REOPENS_TOTAL, "Tasks reopened for another work cycle.");
    metrics::counter!(TASK_REOPENS_TOTAL).absolute(0);
    metrics::describe_counter!(
        TASKS_PARKED_TOTAL,
        "Tasks terminally parked by coordinator safeguards."
    );
    metrics::counter!(TASKS_PARKED_TOTAL).absolute(0);
    metrics::describe_gauge!(
        PR_POLLER_TRACKED,
        "Number of PR-poller clean-merge fast-path tasks currently tracked."
    );
    metrics::gauge!(PR_POLLER_TRACKED).set(0.0);
    metrics::describe_counter!(
        MERGE_FAILURES_TOTAL,
        "PR merge failures that fall back to task reopen/rework."
    );
    metrics::counter!(MERGE_FAILURES_TOTAL).absolute(0);
    for state in SLOT_POOL_STATES {
        metrics::gauge!(SLOT_POOL, "state" => state, "model" => "").set(0.0);
    }
    metrics::gauge!(DISPATCH_COOLDOWNS_ACTIVE).set(0.0);
    metrics::gauge!(DISPATCH_LAST_SUCCESS_TIMESTAMP).set(0.0);
    metrics::gauge!(INFLIGHT_LEDGER_SIZE).set(0.0);
    metrics::gauge!(USER_CAP_UTILIZATION, "user" => "", "model" => "").set(0.0);
    metrics::describe_counter!(
        JIT_PITFALL_HINTS_TOTAL,
        "JIT pitfall hint path observations partitioned by safe outcome labels."
    );
    for outcome in JIT_PITFALL_OUTCOMES {
        metrics::counter!(JIT_PITFALL_HINTS_TOTAL, "outcome" => outcome).absolute(0);
    }
    metrics::describe_gauge!(
        DOCTOR_FINDINGS,
        "Doctor findings observed per check on the most recent recorded run. The only label is the stable DoctorCheck::name() value as check."
    );
    metrics::describe_gauge!(
        DOCTOR_RUN_DURATION_SECONDS,
        "Doctor check run duration in seconds for the most recent recorded run. The only label is the stable DoctorCheck::name() value as check."
    );
    metrics::describe_counter!(
        CARGO_TARGET_SEED_TOTAL,
        "Cargo target seed outcomes partitioned by bounded outcome and fallback_reason labels."
    );
    metrics::counter!(CARGO_TARGET_SEED_TOTAL, "outcome" => "hit", "fallback_reason" => "")
        .absolute(0);
    for reason in [
        cargo_target_seed::FALLBACK_REASON_BASE_MISSING,
        cargo_target_seed::FALLBACK_REASON_BASE_NOT_DIRECTORY,
        cargo_target_seed::FALLBACK_REASON_BASE_UNUSABLE,
        cargo_target_seed::FALLBACK_REASON_SCAN_FAILED,
        cargo_target_seed::FALLBACK_REASON_CLONE_FAILED,
        cargo_target_seed::FALLBACK_REASON_UNKNOWN,
    ] {
        metrics::counter!(
            CARGO_TARGET_SEED_TOTAL,
            "outcome" => "fallback",
            "fallback_reason" => reason
        )
        .absolute(0);
    }
    metrics::describe_counter!(
        CARGO_SEED_HIT_TOTAL,
        "Cargo target warm-base seed successes partitioned by project id."
    );
    metrics::describe_counter!(
        CARGO_SEED_COLD_TOTAL,
        "Cargo target seed fallbacks to a cold private run target dir partitioned by project id and fallback reason."
    );
    metrics::describe_gauge!(
        CARGO_WARM_BASE_FRESHNESS_SECONDS,
        "Seconds elapsed while producing the most recent warm Cargo target base for a project."
    );
    metrics::describe_counter!(
        CARGO_WARM_STEP_TOTAL,
        "Cargo warm-step invocations partitioned by bounded project_id, step, and outcome labels. The free-form cargo argv is intentionally NOT a label; correlate with the djinn_cargo_warm_step workspace path hash gauge and the worker's structured tracing event for full context."
    );
    for outcome in CARGO_WARM_STEP_OUTCOMES {
        metrics::counter!(
            CARGO_WARM_STEP_TOTAL,
            "project_id" => "",
            "step" => "",
            "outcome" => outcome
        )
        .absolute(0);
    }
    metrics::describe_gauge!(
        CARGO_WARM_STEP_WORKSPACE_PATH_HASH,
        "Stable 64-bit FNV-1a hash of the absolute workspace directory the worker resolved for the most recent cargo warm step, partitioned by project_id. Pair with the worker's structured tracing event to recover the actual path without unbounded label cardinality."
    );
    metrics::gauge!(
        CARGO_WARM_STEP_WORKSPACE_PATH_HASH,
        "project_id" => ""
    )
    .set(0.0);
    metrics::describe_gauge!(
        CARGO_WARM_STEP_FRESH_COUNT,
        "Cargo warm-step crate count reported as Fresh, partitioned by project id and step."
    );
    metrics::describe_gauge!(
        CARGO_WARM_STEP_COMPILING_COUNT,
        "Cargo warm-step crate count reported as Compiling, partitioned by project id and step."
    );
}

pub mod dispatch {
    pub const OUTCOME_OK: &str = "ok";
    pub const OUTCOME_COOLDOWN: &str = "cooldown";
    pub const OUTCOME_CAP: &str = "cap";
    pub const OUTCOME_BREAKER: &str = "breaker";
    pub const OUTCOME_ERROR: &str = "error";

    /// Increment the dispatch-attempt counter for one of the stable outcome labels.
    ///
    /// This is intentionally synchronous and non-async so dispatch hot paths never
    /// need to hold any application lock across an await to emit telemetry.
    pub fn increment_attempt(outcome: &'static str) {
        metrics::counter!(super::DISPATCH_ATTEMPTS_TOTAL, "outcome" => outcome).increment(1);
    }

    pub fn record_last_success_now() {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |duration| duration.as_secs_f64());
        metrics::gauge!(super::DISPATCH_LAST_SUCCESS_TIMESTAMP).set(ts);
    }

    pub fn set_cooldowns_active(count: usize) {
        metrics::gauge!(super::DISPATCH_COOLDOWNS_ACTIVE).set(count as f64);
    }

    pub fn set_inflight_ledger_size(count: usize) {
        metrics::gauge!(super::INFLIGHT_LEDGER_SIZE).set(count as f64);
    }

    /// Record a per-user/model cap-utilization ratio.
    ///
    /// `djinn_user_cap_utilization{user,model}` is a single gauge because the
    /// current metrics facade does not expose paired numerator/denominator
    /// samples. The convention is `used / cap`, where `used` is the same
    /// DB-running count overlaid with the coordinator in-flight ledger used for
    /// admission control, and `cap` is the configured per-user/model cap (default
    /// 1). Values may exceed 1.0 if live state was already over cap.
    pub fn set_user_cap_utilization(user: &str, model: &str, used: u32, cap: u32) {
        let cap = cap.max(1);
        let utilization = f64::from(used) / f64::from(cap);
        metrics::gauge!(super::USER_CAP_UTILIZATION, "user" => user.to_owned(), "model" => model.to_owned()).set(utilization);
    }

    pub fn record_success() {
        increment_attempt(OUTCOME_OK);
        record_last_success_now();
    }

    pub fn increment_ok() {
        increment_attempt(OUTCOME_OK);
    }

    pub fn increment_cooldown() {
        increment_attempt(OUTCOME_COOLDOWN);
    }

    pub fn increment_cap() {
        increment_attempt(OUTCOME_CAP);
    }

    pub fn increment_breaker() {
        increment_attempt(OUTCOME_BREAKER);
    }

    pub fn increment_error() {
        increment_attempt(OUTCOME_ERROR);
    }
}

pub mod slot_pool {
    pub const STATE_FREE: &str = "free";
    pub const STATE_BUSY: &str = "busy";

    pub fn set_slots(state: &'static str, model: &str, count: usize) {
        metrics::gauge!(super::SLOT_POOL, "state" => state, "model" => model.to_owned())
            .set(count as f64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn test_guard() -> MutexGuard<'static, ()> {
        TEST_MUTEX.lock().expect("telemetry test mutex poisoned")
    }

    fn rendered_sample<'a>(rendered: &'a str, metric: &str, labels: &[(&str, &str)]) -> &'a str {
        rendered
            .lines()
            .find(|line| {
                line.starts_with(metric)
                    && labels
                        .iter()
                        .all(|(key, value)| line.contains(&format!("{key}=\"{value}\"")))
            })
            .unwrap_or_else(|| panic!("missing sample {metric}{labels:?} in:\n{rendered}"))
    }

    fn unlabelled_sample_value(rendered: &str, metric: &str) -> f64 {
        rendered
            .lines()
            .find_map(|line| {
                line.strip_prefix(metric)
                    .and_then(|suffix| suffix.strip_prefix(' '))
                    .and_then(|value| value.parse::<f64>().ok())
            })
            .unwrap_or_else(|| panic!("missing unlabelled sample {metric} in:\n{rendered}"))
    }

    #[test]
    fn init_is_idempotent_and_registers_dispatch_labels() {
        let _guard = test_guard();
        init().unwrap();
        init().unwrap();

        let rendered = render().unwrap();
        for outcome in DISPATCH_OUTCOMES {
            assert!(
                rendered.contains(&format!(
                    "djinn_dispatch_attempts_total{{outcome=\"{outcome}\"}}"
                )),
                "missing dispatch outcome label {outcome} in:\n{rendered}"
            );
        }
        for outcome in JIT_PITFALL_OUTCOMES {
            assert!(
                rendered.contains(&format!(
                    "djinn_jit_pitfall_hints_total{{outcome=\"{outcome}\"}}"
                )),
                "missing JIT pitfall outcome label {outcome} in:\n{rendered}"
            );
        }
    }

    #[test]
    fn dispatch_attempt_increment_renders_counter() {
        let _guard = test_guard();
        init().unwrap();
        dispatch::increment_ok();

        let rendered = render().unwrap();
        assert!(rendered.contains("djinn_dispatch_attempts_total{outcome=\"ok\"}"));
    }

    #[test]
    fn jit_pitfall_rollout_decision_outcomes_render_separately() {
        let _guard = test_guard();
        init().unwrap();

        jit_pitfalls::increment_outcome(jit_pitfalls::OUTCOME_DISABLED_DEFAULT_OFF);
        jit_pitfalls::increment_outcome(jit_pitfalls::OUTCOME_DISABLED_KILL_SWITCH);
        jit_pitfalls::increment_outcome(jit_pitfalls::OUTCOME_ELIGIBLE_SEARCH);

        let rendered = render().unwrap();
        for outcome in [
            jit_pitfalls::OUTCOME_DISABLED_DEFAULT_OFF,
            jit_pitfalls::OUTCOME_DISABLED_KILL_SWITCH,
            jit_pitfalls::OUTCOME_ELIGIBLE_SEARCH,
        ] {
            assert!(
                rendered.contains(&format!(
                    "djinn_jit_pitfall_hints_total{{outcome=\"{outcome}\"}}"
                )),
                "missing distinct JIT rollout outcome label {outcome} in:\n{rendered}"
            );
        }
    }

    #[test]
    fn live_state_gauges_render_with_bounded_labels() {
        let _guard = test_guard();
        init().unwrap();
        dispatch::set_cooldowns_active(2);
        dispatch::set_inflight_ledger_size(3);
        dispatch::set_user_cap_utilization("user-a", "model-a", 1, 2);
        slot_pool::set_slots(slot_pool::STATE_FREE, "model-a", 4);
        slot_pool::set_slots(slot_pool::STATE_BUSY, "model-a", 5);

        let rendered = render().unwrap();
        assert!(rendered.contains("djinn_dispatch_cooldowns_active 2"));
        assert!(rendered.contains("djinn_inflight_ledger_size 3"));
        assert!(
            rendered_sample(
                &rendered,
                "djinn_user_cap_utilization",
                &[("user", "user-a"), ("model", "model-a")]
            )
            .ends_with(" 0.5"),
            "user-cap gauge must use the documented used/cap ratio convention:\n{rendered}"
        );
        assert!(
            rendered_sample(
                &rendered,
                "djinn_slot_pool",
                &[("state", "free"), ("model", "model-a")]
            )
            .ends_with(" 4")
        );
        assert!(
            rendered_sample(
                &rendered,
                "djinn_slot_pool",
                &[("state", "busy"), ("model", "model-a")]
            )
            .ends_with(" 5")
        );
        assert!(!rendered.contains("slot_id="));
    }

    #[test]
    fn metric_facade_helpers_are_synchronous_unit_functions() {
        let _guard = test_guard();

        fn assert_sync_unit<F: FnOnce()>(f: F) {
            f();
        }

        init().unwrap();
        assert_sync_unit(|| dispatch::increment_attempt(dispatch::OUTCOME_ERROR));
        assert_sync_unit(dispatch::record_last_success_now);
        assert_sync_unit(|| dispatch::set_cooldowns_active(0));
        assert_sync_unit(|| dispatch::set_inflight_ledger_size(0));
        assert_sync_unit(|| dispatch::set_user_cap_utilization("user-sync", "model-sync", 0, 1));
        assert_sync_unit(|| slot_pool::set_slots(slot_pool::STATE_FREE, "model-sync", 0));
        assert_sync_unit(|| {
            jit_pitfalls::increment_outcome(jit_pitfalls::OUTCOME_ELIGIBLE_SEARCH);
        });
        assert_sync_unit(task::increment_reopen);
        assert_sync_unit(task::increment_parked);
        assert_sync_unit(|| pr_poller::set_tracked(0));
        assert_sync_unit(pr_poller::increment_merge_failure);
        assert_sync_unit(breaker::increment_trip);
        assert_sync_unit(|| zombie::increment_reap(zombie::KIND_STALL));
        assert_sync_unit(lead::increment_escalation);
        assert_sync_unit(|| doctor::set_findings("sample.shared_resolver", 1));
        assert_sync_unit(|| doctor::set_run_duration_seconds("sample.shared_resolver", 0.25));
        assert_sync_unit(|| {
            doctor::record_run_duration(
                "sample.shared_resolver",
                std::time::Duration::from_millis(250),
            );
        });
        assert_sync_unit(|| cargo_cache::record_seed_hit("project-sync"));
        assert_sync_unit(|| cargo_cache::record_seed_cold("project-sync", "base_missing"));
        assert_sync_unit(|| cargo_cache::record_warm_base_freshness("project-sync", 1.0));
        assert_sync_unit(|| {
            cargo_warm_step::increment_step(
                "project-sync",
                cargo_warm_step::STEP_CLIPPY,
                cargo_warm_step::OUTCOME_OK,
            );
        });
        assert_sync_unit(|| {
            cargo_warm_step::set_workspace_path("project-sync", "/workspace/x/server");
        });
    }

    #[test]
    fn cargo_seed_hit_counter_renders_project_label() {
        let _guard = test_guard();
        init().unwrap();

        let project_id = "project-seed-hit-test";
        cargo_cache::record_seed_hit(project_id);

        let rendered = render().unwrap();
        let sample = rendered_sample(
            &rendered,
            cargo_cache::SEED_HIT_TOTAL,
            &[("project_id", project_id)],
        );
        assert!(
            sample.ends_with(" 1"),
            "unexpected seed-hit sample: {sample}"
        );
    }

    #[test]
    fn cargo_seed_cold_counter_renders_fallback_reason_label() {
        let _guard = test_guard();
        init().unwrap();

        let project_id = "project-seed-cold-test";
        let fallback_reason = "base target dir is missing";
        cargo_cache::record_seed_cold(project_id, fallback_reason);

        let rendered = render().unwrap();
        let sample = rendered_sample(
            &rendered,
            cargo_cache::SEED_COLD_TOTAL,
            &[
                ("project_id", project_id),
                ("fallback_reason", fallback_reason),
            ],
        );
        assert!(
            sample.ends_with(" 1"),
            "unexpected seed-cold sample: {sample}"
        );
    }

    #[test]
    fn cargo_warm_base_freshness_gauge_renders_positive_value() {
        let _guard = test_guard();
        init().unwrap();

        let project_id = "project-warm-freshness-test";
        cargo_cache::record_warm_base_freshness(project_id, 2.5);

        let rendered = render().unwrap();
        let sample = rendered_sample(
            &rendered,
            cargo_cache::WARM_BASE_FRESHNESS_SECONDS,
            &[("project_id", project_id)],
        );
        let value = sample
            .rsplit_once(' ')
            .and_then(|(_, value)| value.parse::<f64>().ok())
            .expect("freshness gauge sample should end with a number");
        assert!(value > 0.0, "freshness gauge must be positive: {sample}");
    }

    #[test]
    fn cargo_warm_step_counter_renders_bounded_project_step_outcome_labels() {
        let _guard = test_guard();
        init().unwrap();

        let project_id = "project-warm-step-test";
        cargo_warm_step::increment_step(
            project_id,
            cargo_warm_step::STEP_CLIPPY,
            cargo_warm_step::OUTCOME_OK,
        );
        cargo_warm_step::increment_step(
            project_id,
            cargo_warm_step::STEP_TEST_NO_RUN,
            cargo_warm_step::OUTCOME_FAILED,
        );

        let rendered = render().unwrap();
        let clippy = rendered_sample(
            &rendered,
            cargo_warm_step::STEP_TOTAL,
            &[
                ("project_id", project_id),
                ("step", cargo_warm_step::STEP_CLIPPY),
                ("outcome", cargo_warm_step::OUTCOME_OK),
            ],
        );
        assert!(clippy.ends_with(" 1"), "unexpected clippy sample: {clippy}");

        let test_no_run = rendered_sample(
            &rendered,
            cargo_warm_step::STEP_TOTAL,
            &[
                ("project_id", project_id),
                ("step", cargo_warm_step::STEP_TEST_NO_RUN),
                ("outcome", cargo_warm_step::OUTCOME_FAILED),
            ],
        );
        assert!(
            test_no_run.ends_with(" 1"),
            "unexpected test_no_run sample: {test_no_run}"
        );

        // No free-form path or argv may leak into a label — the only label
        // keys we expect on this metric are project_id/step/outcome.
        for line in rendered.lines() {
            if !line.starts_with(cargo_warm_step::STEP_TOTAL) {
                continue;
            }
            for forbidden in ["workspace=", "argv=", "command=", "path="] {
                assert!(
                    !line.contains(forbidden),
                    "cargo_warm_step must not carry free-form path/argv labels: {line}"
                );
            }
        }
    }

    #[test]
    fn cargo_warm_step_workspace_path_hash_gauge_matches_exposed_helper() {
        let _guard = test_guard();
        init().unwrap();

        let project_id = "project-warm-path-hash-test";
        let workspace = "/workspace/proj-warm-path-hash/server";
        cargo_warm_step::set_workspace_path(project_id, workspace);

        let rendered = render().unwrap();
        let sample = rendered_sample(
            &rendered,
            cargo_warm_step::WORKSPACE_PATH_HASH,
            &[("project_id", project_id)],
        );
        let value: f64 = sample
            .rsplit_once(' ')
            .and_then(|(_, value)| value.parse::<f64>().ok())
            .expect("workspace path hash gauge should end with a number");

        let expected = cargo_warm_step::workspace_path_hash(workspace) as f64;
        assert!(
            (value - expected).abs() < f64::EPSILON,
            "gauge value must match the workspace_path_hash helper: gauge={value} expected={expected}"
        );

        // Determinism: hashing the same path twice yields the same value.
        cargo_warm_step::set_workspace_path(project_id, workspace);
        let rendered_again = render().unwrap();
        let sample_again = rendered_sample(
            &rendered_again,
            cargo_warm_step::WORKSPACE_PATH_HASH,
            &[("project_id", project_id)],
        );
        let value_again: f64 = sample_again
            .rsplit_once(' ')
            .and_then(|(_, value)| value.parse::<f64>().ok())
            .expect("workspace path hash gauge should end with a number");
        assert!(
            (value - value_again).abs() < f64::EPSILON,
            "workspace path hash must be deterministic"
        );

        // Different paths must yield different hashes.
        let other_hash = cargo_warm_step::workspace_path_hash("/workspace/other-proj/server");
        assert_ne!(
            value as u64, other_hash,
            "different workspace paths must produce different hashes"
        );
    }

    #[test]
    fn doctor_metrics_render_with_check_only_labels() {
        let _guard = test_guard();
        init().unwrap();

        let check = "sample.shared_resolver";
        doctor::set_findings(check, 3);
        doctor::set_run_duration_seconds(check, 1.25);

        let rendered = render().unwrap();
        assert!(rendered.contains("# HELP djinn_doctor_findings"));
        assert!(rendered.contains("# HELP djinn_doctor_run_duration_seconds"));

        let findings = rendered_sample(&rendered, doctor::FINDINGS, &[("check", check)]);
        assert!(
            findings.ends_with(" 3"),
            "unexpected findings sample: {findings}"
        );
        assert_eq!(
            findings.matches('=').count(),
            1,
            "doctor findings must only render the check label: {findings}"
        );

        let duration =
            rendered_sample(&rendered, doctor::RUN_DURATION_SECONDS, &[("check", check)]);
        assert!(
            duration.ends_with(" 1.25"),
            "unexpected duration sample: {duration}"
        );
        assert_eq!(
            duration.matches('=').count(),
            1,
            "doctor duration must only render the check label: {duration}"
        );

        for forbidden in ["severity=", "entity=", "workspace=", "le="] {
            assert!(
                !findings.contains(forbidden),
                "doctor findings labels must stay bounded: {findings}"
            );
            assert!(
                !duration.contains(forbidden),
                "doctor duration labels must stay bounded: {duration}"
            );
        }
    }

    #[test]
    fn breaker_state_gauge_renders_scope_and_model() {
        let _guard = test_guard();
        init().unwrap();

        breaker::set_state("user-1", "model-a", 0.5);

        let rendered = render().unwrap();
        assert!(rendered.contains("djinn_breaker_state"));
        assert!(rendered.contains("model-a"));
        assert!(rendered.contains("user-1"));
        assert!(rendered.contains(" 0.5"));
    }

    #[test]
    fn zombie_and_lead_counters_render() {
        let _guard = test_guard();
        init().unwrap();

        zombie::increment_reap(zombie::KIND_STARTUP);
        zombie::increment_reap(zombie::KIND_PERIODIC);
        zombie::increment_reap(zombie::KIND_STALL);
        lead::increment_escalation();
        jit_pitfalls::increment_outcome(jit_pitfalls::OUTCOME_INJECTED);

        let rendered = render().unwrap();
        for kind in ZOMBIE_REAP_KINDS {
            assert!(rendered.contains(&format!("djinn_zombie_reaps_total{{kind=\"{kind}\"}}")));
        }
        assert!(rendered.contains("djinn_lead_escalations_total"));
        assert!(rendered.contains("djinn_jit_pitfall_hints_total{outcome=\"injected\"}"));
    }

    #[test]
    fn task_and_pr_poller_metrics_render() {
        let _guard = test_guard();
        init().unwrap();

        let before = render().unwrap();
        let reopens_before = unlabelled_sample_value(&before, TASK_REOPENS_TOTAL);
        let parked_before = unlabelled_sample_value(&before, TASKS_PARKED_TOTAL);
        let merge_failures_before = unlabelled_sample_value(&before, MERGE_FAILURES_TOTAL);

        task::increment_reopen();
        task::increment_parked();
        pr_poller::set_tracked(2);
        pr_poller::increment_merge_failure();

        let rendered = render().unwrap();
        assert_eq!(
            unlabelled_sample_value(&rendered, TASK_REOPENS_TOTAL),
            reopens_before + 1.0
        );
        assert_eq!(
            unlabelled_sample_value(&rendered, TASKS_PARKED_TOTAL),
            parked_before + 1.0
        );
        assert_eq!(unlabelled_sample_value(&rendered, PR_POLLER_TRACKED), 2.0);
        assert_eq!(
            unlabelled_sample_value(&rendered, MERGE_FAILURES_TOTAL),
            merge_failures_before + 1.0
        );
    }

    // ── cargo_target_seed telemetry tests ───────────────────────────

    // Helper to extract the numeric value from a rendered Prometheus sample line.
    fn sample_value(sample: &str) -> f64 {
        sample
            .rsplit_once(' ')
            .and_then(|(_, v)| v.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("sample should end with a number: {sample}"))
    }

    #[test]
    fn cargo_target_seed_hit_counter_renders_outcome_label() {
        let _guard = test_guard();
        init().unwrap();

        let before = render().unwrap();
        let before_sample = rendered_sample(
            &before,
            CARGO_TARGET_SEED_TOTAL,
            &[("outcome", "hit"), ("fallback_reason", "")],
        );
        let before_val = sample_value(before_sample);

        cargo_target_seed::increment_seed_hit();

        let after = render().unwrap();
        let after_sample = rendered_sample(
            &after,
            CARGO_TARGET_SEED_TOTAL,
            &[("outcome", "hit"), ("fallback_reason", "")],
        );
        assert_eq!(
            sample_value(after_sample),
            before_val + 1.0,
            "cargo-target-seed hit should increment by 1: {after_sample}"
        );
    }

    #[test]
    fn cargo_target_seed_fallback_counter_renders_fallback_reason_label() {
        let _guard = test_guard();
        init().unwrap();

        let reason = cargo_target_seed::FALLBACK_REASON_BASE_MISSING;
        let before = render().unwrap();
        let before_sample = rendered_sample(
            &before,
            CARGO_TARGET_SEED_TOTAL,
            &[("outcome", "fallback"), ("fallback_reason", reason)],
        );
        let before_val = sample_value(before_sample);

        cargo_target_seed::increment_seed_fallback(reason);

        let after = render().unwrap();
        let after_sample = rendered_sample(
            &after,
            CARGO_TARGET_SEED_TOTAL,
            &[("outcome", "fallback"), ("fallback_reason", reason)],
        );
        assert_eq!(
            sample_value(after_sample),
            before_val + 1.0,
            "cargo-target-seed fallback should increment by 1: {after_sample}"
        );
    }

    #[test]
    fn cargo_target_seed_fallback_unknown_reason_renders() {
        let _guard = test_guard();
        init().unwrap();

        let reason = cargo_target_seed::FALLBACK_REASON_UNKNOWN;
        let before = render().unwrap();
        let before_sample = rendered_sample(
            &before,
            CARGO_TARGET_SEED_TOTAL,
            &[("outcome", "fallback"), ("fallback_reason", reason)],
        );
        let before_val = sample_value(before_sample);

        cargo_target_seed::increment_seed_fallback(reason);

        let after = render().unwrap();
        let after_sample = rendered_sample(
            &after,
            CARGO_TARGET_SEED_TOTAL,
            &[("outcome", "fallback"), ("fallback_reason", reason)],
        );
        assert_eq!(
            sample_value(after_sample),
            before_val + 1.0,
            "cargo-target-seed unknown-fallback should increment by 1: {after_sample}"
        );
    }

    #[test]
    fn cargo_target_seed_all_fallback_reasons_render_distinctly() {
        let _guard = test_guard();
        init().unwrap();

        let reasons = [
            cargo_target_seed::FALLBACK_REASON_BASE_MISSING,
            cargo_target_seed::FALLBACK_REASON_BASE_NOT_DIRECTORY,
            cargo_target_seed::FALLBACK_REASON_BASE_UNUSABLE,
            cargo_target_seed::FALLBACK_REASON_SCAN_FAILED,
            cargo_target_seed::FALLBACK_REASON_CLONE_FAILED,
            cargo_target_seed::FALLBACK_REASON_UNKNOWN,
        ];

        // Snapshot before values.
        let before = render().unwrap();
        let before_vals: Vec<f64> = reasons
            .iter()
            .map(|reason| {
                let sample = rendered_sample(
                    &before,
                    CARGO_TARGET_SEED_TOTAL,
                    &[("outcome", "fallback"), ("fallback_reason", reason)],
                );
                sample_value(sample)
            })
            .collect();

        for reason in &reasons {
            cargo_target_seed::increment_seed_fallback(reason);
        }

        let after = render().unwrap();
        for (i, reason) in reasons.iter().enumerate() {
            let after_sample = rendered_sample(
                &after,
                CARGO_TARGET_SEED_TOTAL,
                &[("outcome", "fallback"), ("fallback_reason", reason)],
            );
            assert_eq!(
                sample_value(after_sample),
                before_vals[i] + 1.0,
                "cargo-target-seed fallback reason {reason} should increment by 1: {after_sample}"
            );
        }
    }
}

// warm-base validation: v0.6.11 incremental=0 (no-op)
