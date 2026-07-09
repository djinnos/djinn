// Touch to advance main HEAD and trigger a warm job (verification warm-base
// cargo cache validation, 2026-06-16). No behavior change.
//
// djinn:allow-oversize — flat registry of metric definitions; grows by one
// const/helper per new metric. Just over the 50 KiB byte guard.
use std::sync::OnceLock;

use metrics_exporter_prometheus::{BuildError, PrometheusBuilder, PrometheusHandle};

pub const PROMETHEUS_TEXT_CONTENT_TYPE: &str = "text/plain; version=0.0.4";

const DISPATCH_ATTEMPTS_TOTAL: &str = "djinn_dispatch_attempts_total";
const DISPATCH_LAST_SUCCESS_TIMESTAMP: &str = "djinn_dispatch_last_success_timestamp";
const DISPATCH_COOLDOWNS_ACTIVE: &str = "djinn_dispatch_cooldowns_active";
const CROSS_MODEL_REVIEW_TOTAL: &str = "djinn_cross_model_review_total";
const INFLIGHT_LEDGER_SIZE: &str = "djinn_inflight_ledger_size";
const USER_CAP_UTILIZATION: &str = "djinn_user_cap_utilization";
const SLOT_POOL: &str = "djinn_slot_pool";
const DISPATCH_OUTCOMES: [&str; 5] = ["ok", "cooldown", "cap", "breaker", "error"];
const BREAKER_TRIPS_TOTAL: &str = "djinn_breaker_trips_total";
const BREAKER_STATE: &str = "djinn_breaker_state";
const ZOMBIE_REAPS_TOTAL: &str = "djinn_zombie_reaps_total";
const ZOMBIE_REAP_KINDS: [&str; 3] = ["startup", "periodic", "stall"];
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
const INLINE_PR_CLOSED_TOTAL: &str = "djinn_inline_pr_closed_total";
const INLINE_BRANCH_DELETED_TOTAL: &str = "djinn_inline_branch_deleted_total";
const INLINE_CLEANUP_SKIPPED_TOTAL: &str = "djinn_inline_cleanup_skipped_total";
const INLINE_CLEANUP_SKIP_REASONS: [&str; 7] = [
    "merge_queue",
    "grace_period",
    "bot_author",
    "base_of_pr",
    "protected_branch",
    "config_disabled",
    "dry_run",
];

// ─── Stale-PR/branch reconciliation sweep ────────────────────────────────
const STALE_PR_REAPED_TOTAL: &str = "djinn_stale_pr_reaped_total";
const STALE_BRANCH_REAPED_TOTAL: &str = "djinn_stale_branch_reaped_total";
const STALE_PR_SKIPPED_TOTAL: &str = "djinn_stale_pr_skipped_total";
const ORPHAN_WORKER_SESSIONS_REAPED_TOTAL: &str = "djinn_orphan_worker_sessions_reaped_total";

// ─── Coordinator checkpoint preservation gate ─────────────────────────
const PRESERVATION_ATTEMPTS_TOTAL: &str = "djinn_preservation_attempts_total";

// ─── Failover-chain observability ────────────────────────────────────
const FAILOVER_CANDIDATE_ATTEMPTS_TOTAL: &str = "djinn_failover_candidate_attempts_total";
const FAILOVER_CANDIDATE_ACCEPTED_TOTAL: &str = "djinn_failover_candidate_accepted_total";
const FAILOVER_CHAIN_EXHAUSTED_TOTAL: &str = "djinn_failover_chain_exhausted_total";
const FAILOVER_LATENCY_SECONDS: &str = "djinn_failover_latency_seconds";

// ─── Zero-output / stall wall-clock observability ───────────────────
const ZERO_OUTPUT_STALL_SECONDS: &str = "djinn_zero_output_stall_seconds";

// ─── Prompt-context assembly latency observability ──────────────────
const PROMPT_CONTEXT_LATENCY_SECONDS: &str = "djinn_prompt_context_latency_seconds";
const PROMPT_CONTEXT_CHILD_SPAN_LATENCY_SECONDS: &str =
    "djinn_prompt_context_child_span_latency_seconds";

// ─── Arbiter rollout hardening metrics ──────────────────────────────────
const ARBITER_DECISION_TOTAL: &str = "djinn_arbiter_decision_total";
const ARBITER_PARK_TOTAL: &str = "djinn_arbiter_park_total";
const ARBITER_MONITORED_REOPEN_TOTAL: &str = "djinn_arbiter_monitored_reopen_total";
const ARBITER_TERMINATION_TOTAL: &str = "djinn_arbiter_termination_total";
const ARBITER_TIME_IN_ARBITRATION_SECONDS: &str = "djinn_arbiter_time_in_arbitration_seconds";

// ─── Rollout-validation counters (proposal uk2d AC17) ────────────────────
const INFRA_EXEMPT_TOTAL: &str = "djinn_infra_exempt_total";
const FALLBACK_RESCUE_TOTAL: &str = "djinn_fallback_rescue_total";
const REASONING_KILL_TOTAL: &str = "djinn_reasoning_kill_total";

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
    ///
    /// This is a compatibility wrapper that emits the counter with zero-valued
    /// strike-class labels. Prefer [`increment_parked_labeled`] when the caller
    /// has access to the reopen ledger class breakdown.
    pub fn increment_parked() {
        increment_parked_labeled(0, 0, 0, 0);
    }

    /// Increment the parked-task counter with strike-class breakdown labels.
    ///
    /// Labels:
    /// - `quality_strikes` — quality reopen count (review_rejected + merge_queue_failed + other)
    /// - `merge_conflict_reopens` — merge-conflict reopens excluded from quality strikes
    /// - `superseded_reopens` — superseded reopens excluded from quality strikes
    /// - `raw_reopen_count` — total raw reopen count from the task record
    pub fn increment_parked_labeled(
        quality_strikes: i64,
        merge_conflict_reopens: i64,
        superseded_reopens: i64,
        raw_reopen_count: i64,
    ) {
        metrics::counter!(
            super::TASKS_PARKED_TOTAL,
            "quality_strikes" => quality_strikes.to_string(),
            "merge_conflict_reopens" => merge_conflict_reopens.to_string(),
            "superseded_reopens" => superseded_reopens.to_string(),
            "raw_reopen_count" => raw_reopen_count.to_string(),
        )
        .increment(1);
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

pub mod inline_cleanup {
    /// Stable skip-reason labels for the inline cleanup skipped counter.
    pub const REASON_MERGE_QUEUE: &str = "merge_queue";
    pub const REASON_GRACE_PERIOD: &str = "grace_period";
    pub const REASON_BOT_AUTHOR: &str = "bot_author";
    pub const REASON_BASE_OF_PR: &str = "base_of_pr";
    pub const REASON_PROTECTED_BRANCH: &str = "protected_branch";
    pub const REASON_CONFIG_DISABLED: &str = "config_disabled";
    pub const REASON_DRY_RUN: &str = "dry_run";

    /// Increment the inline PR-closed counter when a PR is successfully closed.
    pub fn increment_pr_closed() {
        metrics::counter!(super::INLINE_PR_CLOSED_TOTAL).increment(1);
    }

    /// Increment the inline branch-deleted counter when a branch is successfully deleted.
    pub fn increment_branch_deleted() {
        metrics::counter!(super::INLINE_BRANCH_DELETED_TOTAL).increment(1);
    }

    /// Increment the inline cleanup skipped counter for one of the stable reason labels.
    pub fn increment_skipped(reason: &'static str) {
        metrics::counter!(super::INLINE_CLEANUP_SKIPPED_TOTAL, "reason" => reason).increment(1);
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

    /// Increment the orphan-worker-session reaped counter.
    ///
    /// An orphan worker session is a session whose `status` is `running` but
    /// whose backing task has been closed (or deleted). The periodic sweep
    /// detects and interrupts these sessions.
    pub fn increment_orphan_session_reaped() {
        metrics::counter!(super::ORPHAN_WORKER_SESSIONS_REAPED_TOTAL).increment(1);
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
    metrics::describe_counter!(
        INLINE_PR_CLOSED_TOTAL,
        "Inline PR/branch cleanup: PRs closed by the terminal-close hook."
    );
    metrics::counter!(INLINE_PR_CLOSED_TOTAL).absolute(0);
    metrics::describe_counter!(
        INLINE_BRANCH_DELETED_TOTAL,
        "Inline PR/branch cleanup: branches deleted by the terminal-close hook."
    );
    metrics::counter!(INLINE_BRANCH_DELETED_TOTAL).absolute(0);
    metrics::describe_counter!(
        INLINE_CLEANUP_SKIPPED_TOTAL,
        "Inline PR/branch cleanup: cleanup actions skipped, partitioned by reason."
    );
    for reason in INLINE_CLEANUP_SKIP_REASONS {
        metrics::counter!(INLINE_CLEANUP_SKIPPED_TOTAL, "reason" => reason).absolute(0);
    }
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
    // ─── Failover-chain observability ────────────────────────────────
    metrics::describe_counter!(
        FAILOVER_CANDIDATE_ATTEMPTS_TOTAL,
        "Per-candidate failover-chain dispatch attempts, partitioned by bounded outcome, provider_id, and model_id."
    );
    for outcome in failover::ALL_OUTCOMES {
        metrics::counter!(
            FAILOVER_CANDIDATE_ATTEMPTS_TOTAL,
            "outcome" => outcome,
            "provider_id" => "",
            "model_id" => "",
        )
        .absolute(0);
    }
    metrics::describe_counter!(
        FAILOVER_CANDIDATE_ACCEPTED_TOTAL,
        "Failover candidates that accepted the dispatch, partitioned by provider_id and model_id."
    );
    metrics::counter!(
        FAILOVER_CANDIDATE_ACCEPTED_TOTAL,
        "provider_id" => "",
        "model_id" => "",
    )
    .absolute(0);
    metrics::describe_counter!(
        FAILOVER_CHAIN_EXHAUSTED_TOTAL,
        "Failover chains that exhausted all candidates without acceptance, partitioned by provider_id and model_id of the last tried candidate."
    );
    metrics::counter!(
        FAILOVER_CHAIN_EXHAUSTED_TOTAL,
        "provider_id" => "",
        "model_id" => "",
    )
    .absolute(0);
    metrics::describe_histogram!(
        FAILOVER_LATENCY_SECONDS,
        "Wall-clock elapsed time for a failover-chain traversal from first attempt to terminal event (acceptance or exhaustion)."
    );
    // ─── Zero-output / stall wall-clock observability ────────────────
    metrics::describe_histogram!(
        ZERO_OUTPUT_STALL_SECONDS,
        "Wall-clock time a session spent with zero output before a stall/reap/failover decision was made. Partitioned by timeout_source, failure_class, and chain_exhausted."
    );
    // ─── Prompt-context assembly latency observability ───────────────
    metrics::describe_histogram!(
        PROMPT_CONTEXT_LATENCY_SECONDS,
        "Total wall-clock time for prompt-context assembly across all concurrent phases."
    );
    metrics::describe_histogram!(
        PROMPT_CONTEXT_CHILD_SPAN_LATENCY_SECONDS,
        "Wall-clock time for an individual prompt-context child-span phase. Partitioned by bounded span label."
    );
    // ─── Rollout-validation counters (proposal uk2d AC17) ────────────
    metrics::describe_counter!(
        INFRA_EXEMPT_TOTAL,
        "Infra-exempt attempt outcomes. outcome=(park|quality_strike|total). is_infra distinguishes infra-exempt from quality-strike-classified attempts."
    );
    for outcome in infra_delta::OUTCOMES {
        metrics::counter!(
            INFRA_EXEMPT_TOTAL,
            "outcome" => outcome,
            "is_infra" => "true"
        )
        .absolute(0);
        metrics::counter!(
            INFRA_EXEMPT_TOTAL,
            "outcome" => outcome,
            "is_infra" => "false"
        )
        .absolute(0);
    }
    metrics::describe_counter!(
        FALLBACK_RESCUE_TOTAL,
        "Failover-chain fallback rescue events. Emitted when a later candidate accepts the dispatch after an earlier candidate failed."
    );
    metrics::counter!(FALLBACK_RESCUE_TOTAL).absolute(0);
    metrics::describe_counter!(
        REASONING_KILL_TOTAL,
        "Reasoning-model session outcomes classified by model context, failure class, and outcome (killed/rescued/typed_failure)."
    );
    for fc in reasoning_kill::FAILURE_CLASSES {
        for mc in reasoning_kill::MODEL_CONTEXTS {
            for oc in reasoning_kill::OUTCOMES {
                metrics::counter!(
                    REASONING_KILL_TOTAL,
                    "model_context" => mc,
                    "failure_class" => fc,
                    "outcome" => oc
                )
                .absolute(0);
            }
        }
    }
    // ─── Arbiter rollout hardening metrics ────────────────────────────
    metrics::describe_counter!(
        ARBITER_DECISION_TOTAL,
        "Arbiter decision distribution by decision type. Emitted once per resolved StageOutcome or direct-services decision."
    );
    for decision in arbiter::ALL_DECISIONS {
        metrics::counter!(ARBITER_DECISION_TOTAL, "decision" => decision).absolute(0);
    }
    metrics::describe_counter!(
        ARBITER_PARK_TOTAL,
        "Arbiter park outcomes partitioned by bounded reason and outcome labels."
    );
    for reason in arbiter::ALL_PARK_REASONS {
        for outcome in arbiter::ALL_PARK_OUTCOMES {
            metrics::counter!(
                ARBITER_PARK_TOTAL,
                "reason" => reason,
                "outcome" => outcome
            )
            .absolute(0);
        }
    }
    metrics::describe_counter!(
        ARBITER_MONITORED_REOPEN_TOTAL,
        "Monitored-reopen attempt outcomes from the arbiter reopen decision path."
    );
    for outcome in arbiter::ALL_REOPEN_OUTCOMES {
        metrics::counter!(ARBITER_MONITORED_REOPEN_TOTAL, "outcome" => outcome).absolute(0);
    }
    metrics::describe_counter!(
        ARBITER_TERMINATION_TOTAL,
        "Arbiter session termination events partitioned by infra vs decision-failure class."
    );
    for class in arbiter::ALL_TERMINATION_CLASSES {
        metrics::counter!(ARBITER_TERMINATION_TOTAL, "class" => class).absolute(0);
    }
    metrics::describe_histogram!(
        ARBITER_TIME_IN_ARBITRATION_SECONDS,
        "Wall-clock time a task spent in arbitration from dispatch to decision/park/termination."
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

    /// Cross-model ("Thorough") review outcome at reviewer dispatch.
    ///
    /// `result = "different"` when the reviewer was steered to a model id
    /// distinct from the implementer's; `result = "same_fallback"` when the
    /// review-lane list collapsed to the implementer's model id and dispatch
    /// proceeded same-model. Only emitted when the creator has `diverse_review`
    /// enabled and an implementer model id was known.
    pub fn record_cross_model_review(result: &'static str) {
        metrics::counter!(super::CROSS_MODEL_REVIEW_TOTAL, "result" => result).increment(1);
    }

    pub fn record_last_success_timestamp(timestamp_secs: f64) {
        metrics::gauge!(super::DISPATCH_LAST_SUCCESS_TIMESTAMP).set(timestamp_secs);
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

    pub fn record_success_at(timestamp_secs: f64) {
        increment_attempt(OUTCOME_OK);
        record_last_success_timestamp(timestamp_secs);
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

pub mod preservation {
    //! Coordinator-side checkpoint preservation gate metrics.
    //!
    //! These counters track the outcomes of preservation attempts before
    //! terminal failed/escalated/reap-adjacent session state transitions.
    //! Each outcome label maps to a variant of
    //! `djinn_coordinator::PreservationOutcome`.

    /// Stable outcome labels matching `PreservationOutcome` variants.
    pub const OUTCOME_SUCCEEDED: &str = "succeeded";
    pub const OUTCOME_FAILED: &str = "failed";
    pub const OUTCOME_UNAVAILABLE_WORKER: &str = "unavailable_worker";
    pub const OUTCOME_RUNTIME_UNAVAILABLE: &str = "runtime_unavailable";
    pub const OUTCOME_CLEAN_SKIP: &str = "clean_skip";

    /// Stable trigger labels for the termination path that initiated
    /// the preservation request.
    pub const TRIGGER_STALL: &str = "stall";
    pub const TRIGGER_CEILING: &str = "ceiling";
    pub const TRIGGER_ZOMBIE: &str = "zombie";
    pub const TRIGGER_TERMINAL_FAIL: &str = "terminal_fail";

    /// Increment the preservation-attempt counter for an `(outcome, trigger)`
    /// bucket.  The outcome label is one of the `OUTCOME_*` constants above;
    /// the trigger label is one of the `TRIGGER_*` constants.
    pub fn increment_attempt(outcome: &'static str, trigger: &'static str) {
        metrics::counter!(
            super::PRESERVATION_ATTEMPTS_TOTAL,
            "outcome" => outcome,
            "trigger" => trigger,
        )
        .increment(1);
    }
}

pub mod failover {
    //! Failover-chain observability metrics.
    //!
    //! These counters and the latency histogram track per-candidate attempt,
    //! acceptance, and chain-exhaustion outcomes during coordinator failover-
    //! chain traversal. Labels are intentionally bounded to keep Prometheus
    //! cardinality under control:
    //!
    //! - `provider_id` / `model_id` — candidate identity (bounded by the
    //!   catalog size, typically single-digit).
    //! - `outcome` — one of the `OUTCOME_*` constants below (3 variants).
    //!
    //! High-cardinality dimensions that MUST NOT appear as Prometheus labels:
    //!
    //! - `task_id` — available in the tracing span (`djinn.dispatch.task_id`)
    //!   and in structured log fields emitted by
    //!   [`djinn_coordinator::dispatch::lane_resolution_log`].
    //! - `session_id` — present in tracing fields when the dispatch path has
    //!   access to an active session; likewise belongs in structured logs, not
    //!   in metric labels.
    //! - `candidate_index` — recorded in tracing/log fields for per-candidate
    //!   detail; its range is bounded but including it as a Prometheus label
    //!   would multiply series by max-candidates-per-chain.
    //!
    //! Callers should emit complementary `tracing` events (the existing
    //! `failover_candidate_attempt`, `failover_candidate_accepted`, and a new
    //! `failover_chain_exhausted` event) so that task-scoped drill-down is
    //! available via structured-log queries without exploding metric series.

    /// Bounded outcome labels for `djinn_failover_candidate_attempts_total`.
    pub const OUTCOME_BREAKER_OPEN: &str = "breaker_open";
    pub const OUTCOME_AT_CAPACITY: &str = "at_capacity";
    pub const OUTCOME_ERROR: &str = "error";

    /// All bounded outcome labels — used for registration seeding.
    pub(crate) const ALL_OUTCOMES: [&str; 3] =
        [OUTCOME_BREAKER_OPEN, OUTCOME_AT_CAPACITY, OUTCOME_ERROR];

    /// Increment the per-candidate failover attempt counter.
    ///
    /// `outcome` MUST be one of the `OUTCOME_*` constants above.
    /// `provider_id` and `model_id` are the candidate's parsed identifiers.
    ///
    /// This is intentionally synchronous and non-async so failover traversal
    /// hot paths never need to hold any application lock across an await.
    pub fn increment_candidate_attempt(outcome: &'static str, provider_id: &str, model_id: &str) {
        metrics::counter!(
            super::FAILOVER_CANDIDATE_ATTEMPTS_TOTAL,
            "outcome" => outcome,
            "provider_id" => provider_id.to_owned(),
            "model_id" => model_id.to_owned(),
        )
        .increment(1);
    }

    /// Increment the failover candidate accepted counter.
    ///
    /// Emitted when the first candidate in the failover chain that accepts
    /// the dispatch is found.
    pub fn increment_candidate_accepted(provider_id: &str, model_id: &str) {
        metrics::counter!(
            super::FAILOVER_CANDIDATE_ACCEPTED_TOTAL,
            "provider_id" => provider_id.to_owned(),
            "model_id" => model_id.to_owned(),
        )
        .increment(1);
    }

    /// Increment the failover chain exhausted counter.
    ///
    /// Emitted once per dispatch attempt when all failover candidates have
    /// been tried and none accepted the dispatch.
    pub fn increment_chain_exhausted(provider_id: &str, model_id: &str) {
        metrics::counter!(
            super::FAILOVER_CHAIN_EXHAUSTED_TOTAL,
            "provider_id" => provider_id.to_owned(),
            "model_id" => model_id.to_owned(),
        )
        .increment(1);
    }

    /// Record the elapsed wall-clock time for a failover-chain traversal.
    ///
    /// Called once per dispatch attempt: either when a candidate is accepted
    /// (successful chain) or when the chain is fully exhausted. The duration
    /// spans from the first candidate attempt to the terminal event.
    pub fn record_latency(latency: std::time::Duration) {
        metrics::histogram!(super::FAILOVER_LATENCY_SECONDS).record(latency);
    }
}

pub mod liveness_metrics {
    //! Zero-output / stall wall-clock observability metrics.
    //!
    //! These histograms track the wall-clock time a session spent with zero
    //! output before a stall/reap/failover decision was made. Labels are
    //! intentionally bounded to keep Prometheus cardinality under control:
    //!
    //! - `timeout_source` — why the decision fired (`first_call_hang` or
    //!   `idle_stall`); bounded, closed set.
    //! - `failure_class` — the liveness failure class (e.g.
    //!   `first_call_hang`, `idle_stall`); bounded by the set of failure
    //!   classes the coordinator produces.
    //! - `chain_exhausted` — `"true"` or `"false"`; O(1) cardinality.
    //!
    //! High-cardinality dimensions (`task_id`, `session_id`, `provider_id`,
    //! `model_id`) belong in structured tracing fields emitted at the
    //! decision site, not in Prometheus labels.

    /// Stable timeout-source labels for the zero-output stall histogram.
    pub const TIMEOUT_SOURCE_FIRST_CALL_HANG: &str = "first_call_hang";
    pub const TIMEOUT_SOURCE_IDLE_STALL: &str = "idle_stall";

    /// Stable failure-class labels.
    pub const FAILURE_CLASS_FIRST_CALL_HANG: &str = "first_call_hang";
    pub const FAILURE_CLASS_IDLE_STALL: &str = "idle_stall";

    /// Record the wall-clock time a session spent with zero output before a
    /// stall/kill/reap decision was made.
    ///
    /// `duration` is the elapsed wall-clock from session start (or last
    /// activity) to the decision point. `timeout_source` is one of the
    /// `TIMEOUT_SOURCE_*` constants above; `failure_class` is one of the
    /// `FAILURE_CLASS_*` constants. `chain_exhausted` indicates whether the
    /// failover chain was exhausted for this decision.
    ///
    /// This is intentionally synchronous and non-async so stall-decision
    /// hot paths never need to hold any application lock across an await.
    pub fn record_zero_output_stall(
        duration: std::time::Duration,
        timeout_source: &'static str,
        failure_class: &'static str,
        chain_exhausted: bool,
    ) {
        metrics::histogram!(
            super::ZERO_OUTPUT_STALL_SECONDS,
            "timeout_source" => timeout_source,
            "failure_class" => failure_class,
            "chain_exhausted" => if chain_exhausted { "true" } else { "false" },
        )
        .record(duration);
    }
}

pub mod prompt_context_metrics {
    //! Prompt-context assembly latency observability metrics.
    //!
    //! These histograms track the wall-clock time spent assembling the
    //! prompt context for a worker/reviewer/planner session. Two levels:
    //!
    //! - **Total** — wall-clock for the entire `assemble_prompt_context`
    //!   call (all phases, including concurrency).
    //! - **Child span** — wall-clock for an individual child-span phase
    //!   (activity_db, epic_context, knowledge_context, attempt_history,
    //!   code_graph, reviewer_diff).
    //!
    //! The child-span metric carries a bounded `span` label whose value
    //! is one of the stable span-name constants below. No other labels
    //! are added — `task_id` is in the tracing span, not in the metric.

    /// Stable child-span labels for the prompt-context child-span
    /// latency histogram.
    pub const SPAN_ACTIVITY_DB: &str = "activity_db";
    pub const SPAN_EPIC_CONTEXT: &str = "epic_context";
    pub const SPAN_KNOWLEDGE_CONTEXT: &str = "knowledge_context";
    pub const SPAN_ATTEMPT_HISTORY: &str = "attempt_history";
    pub const SPAN_CODE_GRAPH: &str = "code_graph";
    pub const SPAN_REVIEWER_DIFF: &str = "reviewer_diff";

    /// All bounded child-span labels — used for registration seeding and tests.
    #[cfg(test)]
    pub(crate) const ALL_SPANS: [&str; 6] = [
        SPAN_ACTIVITY_DB,
        SPAN_EPIC_CONTEXT,
        SPAN_KNOWLEDGE_CONTEXT,
        SPAN_ATTEMPT_HISTORY,
        SPAN_CODE_GRAPH,
        SPAN_REVIEWER_DIFF,
    ];

    /// Record the total wall-clock time for prompt-context assembly.
    pub fn record_total(duration: std::time::Duration) {
        metrics::histogram!(super::PROMPT_CONTEXT_LATENCY_SECONDS).record(duration);
    }

    /// Record the wall-clock time for an individual child-span phase.
    ///
    /// `span_name` MUST be one of the `SPAN_*` constants above.
    pub fn record_child_span(span_name: &'static str, duration: std::time::Duration) {
        metrics::histogram!(
            super::PROMPT_CONTEXT_CHILD_SPAN_LATENCY_SECONDS,
            "span" => span_name,
        )
        .record(duration);
    }
}

pub mod infra_delta {
    //! Infra-exempt attempt observability.
    //!
    //! Tracks outcomes where infra-classified failures (`timed_out`,
    //! `spawn_failed`, `crashed`) are excluded from quality-strike and park
    //! escalation counters. The `is_infra` label distinguishes infra-exempt
    //! attempts from quality-strike-classified attempts within the same
    //! outcome bucket, enabling delta computation on dashboards.
    //!
    //! Bounded labels:
    //! - `outcome` — one of `OUTCOME_PARK`, `OUTCOME_QUALITY_STRIKE`,
    //!   `OUTCOME_TOTAL`
    //! - `is_infra` — `"true"` for infra-exempt, `"false"` for quality-strike

    pub const OUTCOME_PARK: &str = "park";
    pub const OUTCOME_QUALITY_STRIKE: &str = "quality_strike";
    pub const OUTCOME_TOTAL: &str = "total";

    pub(crate) const OUTCOMES: [&str; 3] = [OUTCOME_PARK, OUTCOME_QUALITY_STRIKE, OUTCOME_TOTAL];

    /// Increment the infra-exempt counter for the given outcome and infra
    /// classification.
    ///
    /// `outcome` MUST be one of the `OUTCOME_*` constants above.
    /// `is_infra` is `true` when the attempt was infra-classified (exempt from
    /// quality-strike counting), `false` when it was classified as a
    /// quality-strike-class outcome.
    pub fn increment(outcome: &'static str, is_infra: bool) {
        metrics::counter!(
            super::INFRA_EXEMPT_TOTAL,
            "outcome" => outcome,
            "is_infra" => if is_infra { "true" } else { "false" },
        )
        .increment(1);
    }
}

pub mod fallback_rescue {
    //! Fallback-rescue rate observability.
    //!
    //! Emitted when a failover chain's later candidate accepts the dispatch
    //! after one or more earlier candidates failed. Preserves existing
    //! guarantees: rescued sessions are not suspended or quality-struck.
    //!
    //! Bounded labels: none (single counter, no labels needed).

    /// Increment the fallback-rescue counter.
    ///
    /// Called on the success path of `try_dispatch_to_pool` when the accepted
    /// candidate is not the first in the chain (i.e. `candidate_index > 0`),
    /// indicating that the dispatch was rescued by a later candidate after
    /// earlier candidates failed.
    pub fn increment_rescue() {
        metrics::counter!(super::FALLBACK_RESCUE_TOTAL).increment(1);
    }
}

pub mod reasoning_kill {
    //! Reasoning-model false-positive kill observability.
    //!
    //! Classifies reasoning-model session outcomes by model context, failure
    //! class, and outcome so operators can track whether reasoning models are
    //! being disproportionately killed by stall timeouts (especially the
    //! first-call-hang detection, which targets backend latency rather than
    //! genuine hangs), rescued by fallback candidates, or experiencing typed
    //! failures.
    //!
    //! Bounded labels:
    //! - `model_context` — `"reasoning"` or `"non_reasoning"`
    //! - `failure_class` — `"first_call_hang"` or `"idle_stall"`
    //! - `outcome` — `"killed"`, `"rescued"`, or `"typed_failure"`

    pub const MODEL_CONTEXT_REASONING: &str = "reasoning";
    pub const MODEL_CONTEXT_NON_REASONING: &str = "non_reasoning";

    pub const FAILURE_CLASS_FIRST_CALL_HANG: &str = "first_call_hang";
    pub const FAILURE_CLASS_IDLE_STALL: &str = "idle_stall";

    pub const OUTCOME_KILLED: &str = "killed";
    pub const OUTCOME_RESCUED: &str = "rescued";
    pub const OUTCOME_TYPED_FAILURE: &str = "typed_failure";

    pub(crate) const MODEL_CONTEXTS: [&str; 2] =
        [MODEL_CONTEXT_REASONING, MODEL_CONTEXT_NON_REASONING];
    pub(crate) const FAILURE_CLASSES: [&str; 2] =
        [FAILURE_CLASS_FIRST_CALL_HANG, FAILURE_CLASS_IDLE_STALL];
    pub(crate) const OUTCOMES: [&str; 3] = [OUTCOME_KILLED, OUTCOME_RESCUED, OUTCOME_TYPED_FAILURE];

    /// Heuristic check whether a `model_id` string refers to a known
    /// reasoning-capable model.
    ///
    /// Checks the model name portion (after the provider prefix) for
    /// substrings matching known reasoning model families. This is a
    /// conservative heuristic — it errs on the side of `"non_reasoning"`
    /// when uncertain so false-positive kill counts remain accurate.
    ///
    /// Known reasoning families (from the provider catalog):
    /// - `mimo` (MiMo-V2.5-Pro, MiMo-V2.5)
    /// - `glm` (GLM-5)
    /// - `thinking` suffix (kimi-k2-thinking)
    /// - `o1` / `o3` prefixes (OpenAI reasoning models)
    pub fn is_reasoning_model(model_id: &str) -> bool {
        let name = model_id.rsplit('/').next().unwrap_or(model_id);
        let lower = name.to_lowercase();
        lower.starts_with("o1")
            || lower.starts_with("o3")
            || lower.starts_with("mimo")
            || lower.starts_with("glm")
            || lower.contains("thinking")
    }

    /// Emit a reasoning-model outcome observation with all three bounded
    /// labels: model context, failure class, and outcome.
    ///
    /// `model_context` MUST be one of `MODEL_CONTEXT_REASONING` or
    /// `MODEL_CONTEXT_NON_REASONING`.
    /// `failure_class` MUST be one of `FAILURE_CLASS_FIRST_CALL_HANG` or
    /// `FAILURE_CLASS_IDLE_STALL`.
    /// `outcome` MUST be one of `OUTCOME_KILLED`, `OUTCOME_RESCUED`, or
    /// `OUTCOME_TYPED_FAILURE`.
    pub fn increment(
        model_context: &'static str,
        failure_class: &'static str,
        outcome: &'static str,
    ) {
        metrics::counter!(
            super::REASONING_KILL_TOTAL,
            "model_context" => model_context,
            "failure_class" => failure_class,
            "outcome" => outcome,
        )
        .increment(1);
    }
}

/// Arbiter rollout hardening metrics.
///
/// Covers the two-week rollout signal set: decision distribution,
/// park outcome/reason, monitored reopen outcome, termination class,
/// and wall-clock time-in-arbitration.
///
/// All labels are bounded/enumerated. Unbounded details belong in
/// structured activity payloads, not metric labels.
pub mod arbiter {
    // ── Decision distribution ──────────────────────────────────────────
    pub const DECISION_APPROVE: &str = "approve";
    pub const DECISION_APPROVE_CONFLICT: &str = "approve_conflict";
    pub const DECISION_REOPEN: &str = "reopen";
    pub const DECISION_PARK: &str = "park";
    pub const DECISION_ESCALATE: &str = "escalate";
    pub const DECISION_DECOMPOSE: &str = "decompose";
    pub const DECISION_FORCE_CLOSE: &str = "force_close";

    /// All valid decision labels (bounded set for cardinality guard).
    pub const ALL_DECISIONS: [&str; 7] = [
        DECISION_APPROVE,
        DECISION_APPROVE_CONFLICT,
        DECISION_REOPEN,
        DECISION_PARK,
        DECISION_ESCALATE,
        DECISION_DECOMPOSE,
        DECISION_FORCE_CLOSE,
    ];

    // ── Park outcome ───────────────────────────────────────────────────
    pub const PARK_OUTCOME_SUCCESS: &str = "success";
    pub const PARK_OUTCOME_TRANSITION_FAILED: &str = "transition_failed";
    pub const PARK_OUTCOME_RECOVERY: &str = "recovery";

    pub const ALL_PARK_OUTCOMES: [&str; 3] = [
        PARK_OUTCOME_SUCCESS,
        PARK_OUTCOME_TRANSITION_FAILED,
        PARK_OUTCOME_RECOVERY,
    ];

    // ── Park reason ────────────────────────────────────────────────────
    pub const PARK_REASON_DEADLINE_EXPIRED: &str = "deadline_expired";
    pub const PARK_REASON_DECISION_FAILURE_CAP: &str = "decision_failure_cap";
    pub const PARK_REASON_ARBITER_DECIDED: &str = "arbiter_decided";
    pub const PARK_REASON_CONSUMED_REENTRY: &str = "consumed_reentry";
    pub const PARK_REASON_ARBITRATION_ERROR: &str = "arbitration_error";

    pub const ALL_PARK_REASONS: [&str; 5] = [
        PARK_REASON_DEADLINE_EXPIRED,
        PARK_REASON_DECISION_FAILURE_CAP,
        PARK_REASON_ARBITER_DECIDED,
        PARK_REASON_CONSUMED_REENTRY,
        PARK_REASON_ARBITRATION_ERROR,
    ];

    // ── Monitored reopen outcome ───────────────────────────────────────
    pub const REOPEN_OUTCOME_STARTED: &str = "started";
    pub const REOPEN_OUTCOME_NO_UNCONSUMED: &str = "no_unconsumed";
    pub const REOPEN_OUTCOME_FAILED: &str = "failed";

    pub const ALL_REOPEN_OUTCOMES: [&str; 3] = [
        REOPEN_OUTCOME_STARTED,
        REOPEN_OUTCOME_NO_UNCONSUMED,
        REOPEN_OUTCOME_FAILED,
    ];

    // ── Termination class ──────────────────────────────────────────────
    pub const TERMINATION_INFRA: &str = "infra";
    pub const TERMINATION_DECISION_FAILURE: &str = "decision_failure";

    pub const ALL_TERMINATION_CLASSES: [&str; 2] =
        [TERMINATION_INFRA, TERMINATION_DECISION_FAILURE];

    /// Record an arbiter decision event.  Call once per resolved
    /// `StageOutcome` decision from the supervisor or from
    /// `record_arbiter_decision` / `execute_arbiter_park_transaction` /
    /// `start_monitored_reopen` in the services layer.
    ///
    /// `decision` must be one of the `DECISION_*` constants.
    pub fn record_decision(decision: &'static str) {
        metrics::counter!(super::ARBITER_DECISION_TOTAL, "decision" => decision).increment(1);
    }

    /// Record an arbiter park outcome.  Call once per park path
    /// (coordinator deadline auto-park, decision-failure-cap park,
    /// consumed-reentry park, arbiter-decided park, or arbitration
    /// error park).
    ///
    /// `reason` and `outcome` must be from the `PARK_REASON_*` and
    /// `PARK_OUTCOME_*` constant sets respectively.
    pub fn record_park(reason: &'static str, outcome: &'static str) {
        metrics::counter!(
            super::ARBITER_PARK_TOTAL,
            "reason" => reason,
            "outcome" => outcome
        )
        .increment(1);
    }

    /// Record a monitored-reopen outcome.  Call from
    /// `start_monitored_reopen` (started / no_unconsumed / failed).
    pub fn record_monitored_reopen(outcome: &'static str) {
        metrics::counter!(
            super::ARBITER_MONITORED_REOPEN_TOTAL,
            "outcome" => outcome
        )
        .increment(1);
    }

    /// Record an arbiter session termination class.  Call from
    /// `record_arbiter_session_termination`.
    ///
    /// `class` must be one of `TERMINATION_INFRA` or
    /// `TERMINATION_DECISION_FAILURE`.
    pub fn record_termination(class: &'static str) {
        metrics::counter!(
            super::ARBITER_TERMINATION_TOTAL,
            "class" => class
        )
        .increment(1);
    }

    /// Record the wall-clock time a task spent in arbitration.
    ///
    /// `seconds` is the elapsed time from arbitration row creation to
    /// the current decision / park / termination event.
    pub fn record_time_in_arbitration(seconds: f64) {
        metrics::histogram!(super::ARBITER_TIME_IN_ARBITRATION_SECONDS).record(seconds);
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

    /// Like [`rendered_sample`] but parses the trailing numeric value.
    fn labeled_sample_value(rendered: &str, metric: &str, labels: &[(&str, &str)]) -> f64 {
        let line = rendered_sample(rendered, metric, labels);
        line.rsplit_once(' ')
            .and_then(|(_, v)| v.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("labeled sample should end with a number: {line}"))
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
        assert_sync_unit(|| dispatch::record_last_success_timestamp(1.0));
        assert_sync_unit(|| dispatch::set_cooldowns_active(0));
        assert_sync_unit(|| dispatch::set_inflight_ledger_size(0));
        assert_sync_unit(|| dispatch::set_user_cap_utilization("user-sync", "model-sync", 0, 1));
        assert_sync_unit(|| slot_pool::set_slots(slot_pool::STATE_FREE, "model-sync", 0));
        assert_sync_unit(|| {
            jit_pitfalls::increment_outcome(jit_pitfalls::OUTCOME_ELIGIBLE_SEARCH);
        });
        assert_sync_unit(task::increment_reopen);
        assert_sync_unit(task::increment_parked);
        assert_sync_unit(|| task::increment_parked_labeled(2, 1, 0, 5));
        assert_sync_unit(|| pr_poller::set_tracked(0));
        assert_sync_unit(pr_poller::increment_merge_failure);
        assert_sync_unit(breaker::increment_trip);
        assert_sync_unit(|| zombie::increment_reap(zombie::KIND_STALL));
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
        assert_sync_unit(inline_cleanup::increment_pr_closed);
        assert_sync_unit(inline_cleanup::increment_branch_deleted);
        assert_sync_unit(|| inline_cleanup::increment_skipped(inline_cleanup::REASON_DRY_RUN));
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
    fn zombie_and_jit_counters_render() {
        let _guard = test_guard();
        init().unwrap();

        zombie::increment_reap(zombie::KIND_STARTUP);
        zombie::increment_reap(zombie::KIND_PERIODIC);
        zombie::increment_reap(zombie::KIND_STALL);
        jit_pitfalls::increment_outcome(jit_pitfalls::OUTCOME_INJECTED);

        let rendered = render().unwrap();
        for kind in ZOMBIE_REAP_KINDS {
            assert!(rendered.contains(&format!("djinn_zombie_reaps_total{{kind=\"{kind}\"}}")));
        }
        assert!(rendered.contains("djinn_jit_pitfall_hints_total{outcome=\"injected\"}"));
    }

    #[test]
    fn task_and_pr_poller_metrics_render() {
        let _guard = test_guard();
        init().unwrap();

        // Prime the parked counter so the "before" snapshot contains the
        // labeled metric line.
        task::increment_parked();

        let before = render().unwrap();
        let reopens_before = unlabelled_sample_value(&before, TASK_REOPENS_TOTAL);
        let parked_labels = &[
            ("quality_strikes", "0"),
            ("merge_conflict_reopens", "0"),
            ("superseded_reopens", "0"),
            ("raw_reopen_count", "0"),
        ];
        let parked_before = labeled_sample_value(&before, TASKS_PARKED_TOTAL, parked_labels);
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
            labeled_sample_value(&rendered, TASKS_PARKED_TOTAL, parked_labels),
            parked_before + 1.0
        );
        assert_eq!(unlabelled_sample_value(&rendered, PR_POLLER_TRACKED), 2.0);
        assert_eq!(
            unlabelled_sample_value(&rendered, MERGE_FAILURES_TOTAL),
            merge_failures_before + 1.0
        );
    }

    #[test]
    fn parked_labeled_metric_renders_breakdown_labels() {
        let _guard = test_guard();
        init().unwrap();

        task::increment_parked_labeled(3, 1, 2, 7);

        let rendered = render().unwrap();
        let value = labeled_sample_value(
            &rendered,
            TASKS_PARKED_TOTAL,
            &[
                ("quality_strikes", "3"),
                ("merge_conflict_reopens", "1"),
                ("superseded_reopens", "2"),
                ("raw_reopen_count", "7"),
            ],
        );
        assert_eq!(value, 1.0);
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

    // ── failover-chain telemetry tests ───────────────────────────────

    #[test]
    fn failover_metric_facade_helpers_are_synchronous_unit_functions() {
        let _guard = test_guard();

        fn assert_sync_unit<F: FnOnce()>(f: F) {
            f();
        }

        init().unwrap();
        assert_sync_unit(|| {
            failover::increment_candidate_attempt(
                failover::OUTCOME_BREAKER_OPEN,
                "test-provider",
                "test-model",
            );
        });
        assert_sync_unit(|| {
            failover::increment_candidate_accepted("test-provider", "test-model");
        });
        assert_sync_unit(|| {
            failover::increment_chain_exhausted("test-provider", "test-model");
        });
        assert_sync_unit(|| {
            failover::record_latency(std::time::Duration::from_millis(250));
        });
    }

    #[test]
    fn failover_candidate_attempts_render_outcome_and_model_labels() {
        let _guard = test_guard();
        init().unwrap();

        failover::increment_candidate_attempt(
            failover::OUTCOME_BREAKER_OPEN,
            "provider-a",
            "model-a",
        );
        failover::increment_candidate_attempt(
            failover::OUTCOME_AT_CAPACITY,
            "provider-b",
            "model-b",
        );
        failover::increment_candidate_attempt(failover::OUTCOME_ERROR, "provider-c", "model-c");

        let rendered = render().unwrap();
        for outcome in failover::ALL_OUTCOMES {
            assert!(
                rendered.contains(FAILOVER_CANDIDATE_ATTEMPTS_TOTAL),
                "missing {FAILOVER_CANDIDATE_ATTEMPTS_TOTAL} in rendered output:\n{rendered}",
            );
            let sample = rendered_sample(
                &rendered,
                FAILOVER_CANDIDATE_ATTEMPTS_TOTAL,
                &[("outcome", outcome), ("provider_id", ""), ("model_id", "")],
            );
            // At minimum the zero-seeded sample must exist.
            let _ = sample;
        }
        // Our real samples should also be present.
        assert!(
            rendered_sample(
                &rendered,
                FAILOVER_CANDIDATE_ATTEMPTS_TOTAL,
                &[
                    ("outcome", failover::OUTCOME_BREAKER_OPEN),
                    ("provider_id", "provider-a"),
                    ("model_id", "model-a"),
                ],
            )
            .ends_with(" 1"),
        );
        assert!(
            rendered_sample(
                &rendered,
                FAILOVER_CANDIDATE_ATTEMPTS_TOTAL,
                &[
                    ("outcome", failover::OUTCOME_AT_CAPACITY),
                    ("provider_id", "provider-b"),
                    ("model_id", "model-b"),
                ],
            )
            .ends_with(" 1"),
        );
        assert!(
            rendered_sample(
                &rendered,
                FAILOVER_CANDIDATE_ATTEMPTS_TOTAL,
                &[
                    ("outcome", failover::OUTCOME_ERROR),
                    ("provider_id", "provider-c"),
                    ("model_id", "model-c"),
                ],
            )
            .ends_with(" 1"),
        );
    }

    #[test]
    fn failover_candidate_accepted_renders_model_labels() {
        let _guard = test_guard();
        init().unwrap();

        failover::increment_candidate_accepted("provider-x", "model-y");

        let rendered = render().unwrap();
        let sample = rendered_sample(
            &rendered,
            FAILOVER_CANDIDATE_ACCEPTED_TOTAL,
            &[("provider_id", "provider-x"), ("model_id", "model-y")],
        );
        assert!(
            sample.ends_with(" 1"),
            "unexpected accepted sample: {sample}"
        );
    }

    #[test]
    fn failover_chain_exhausted_renders_model_labels() {
        let _guard = test_guard();
        init().unwrap();

        failover::increment_chain_exhausted("provider-z", "model-w");

        let rendered = render().unwrap();
        let sample = rendered_sample(
            &rendered,
            FAILOVER_CHAIN_EXHAUSTED_TOTAL,
            &[("provider_id", "provider-z"), ("model_id", "model-w")],
        );
        assert!(
            sample.ends_with(" 1"),
            "unexpected exhausted sample: {sample}"
        );
    }

    #[test]
    fn failover_latency_histogram_renders_after_recording() {
        let _guard = test_guard();
        init().unwrap();

        failover::record_latency(std::time::Duration::from_millis(150));
        failover::record_latency(std::time::Duration::from_millis(4500));

        let rendered = render().unwrap();
        assert!(
            rendered.contains(FAILOVER_LATENCY_SECONDS),
            "missing {FAILOVER_LATENCY_SECONDS} in rendered output:\n{rendered}",
        );
        assert!(
            rendered.contains(&format!("# HELP {FAILOVER_LATENCY_SECONDS}")),
            "missing HELP line for latency summary:\n{rendered}",
        );
        // The metrics-exporter-prometheus crate renders `histogram!` metrics
        // as DDSketch summaries (with quantiles) rather than classic
        // histogram buckets.
        assert!(
            rendered.contains(&format!("# TYPE {FAILOVER_LATENCY_SECONDS} summary")),
            "missing TYPE line for latency summary:\n{rendered}",
        );
    }

    #[test]
    fn failover_metrics_registered_labels_render_at_zero_on_init() {
        let _guard = test_guard();
        init().unwrap();

        let rendered = render().unwrap();
        // After init, the zero-seeded samples should be present so that
        // the metric is visible even before the first real event.
        for outcome in failover::ALL_OUTCOMES {
            assert!(
                rendered.contains(&format!(
                    "{FAILOVER_CANDIDATE_ATTEMPTS_TOTAL}{{outcome=\"{outcome}\",provider_id=\"\",model_id=\"\"}} 0"
                )),
                "missing zero-seeded attempt sample for outcome={outcome}:\n{rendered}",
            );
        }
        assert!(
            rendered.contains(&format!(
                "{FAILOVER_CANDIDATE_ACCEPTED_TOTAL}{{provider_id=\"\",model_id=\"\"}} 0"
            )),
            "missing zero-seeded accepted sample:\n{rendered}",
        );
        assert!(
            rendered.contains(&format!(
                "{FAILOVER_CHAIN_EXHAUSTED_TOTAL}{{provider_id=\"\",model_id=\"\"}} 0"
            )),
            "missing zero-seeded exhausted sample:\n{rendered}",
        );
    }

    #[test]
    fn failover_metrics_do_not_contain_high_cardinality_labels() {
        let _guard = test_guard();
        init().unwrap();

        failover::increment_candidate_attempt(
            failover::OUTCOME_BREAKER_OPEN,
            "provider-check",
            "model-check",
        );
        failover::record_latency(std::time::Duration::from_millis(100));

        let rendered = render().unwrap();
        for forbidden in ["task_id=", "session_id=", "candidate_index="] {
            for line in rendered.lines() {
                if !line.starts_with(FAILOVER_CANDIDATE_ATTEMPTS_TOTAL)
                    && !line.starts_with(FAILOVER_CANDIDATE_ACCEPTED_TOTAL)
                    && !line.starts_with(FAILOVER_CHAIN_EXHAUSTED_TOTAL)
                    && !line.starts_with(FAILOVER_LATENCY_SECONDS)
                {
                    continue;
                }
                assert!(
                    !line.contains(forbidden),
                    "failover metric must not carry high-cardinality label {forbidden}: {line}",
                );
            }
        }
    }

    // ─── zero-output / stall observability tests ──────────────────

    #[test]
    fn zero_output_stall_metric_facade_helpers_are_synchronous_unit_functions() {
        let _guard = test_guard();

        fn assert_sync_unit<F: FnOnce()>(f: F) {
            f();
        }

        init().unwrap();
        assert_sync_unit(|| {
            liveness_metrics::record_zero_output_stall(
                std::time::Duration::from_secs(120),
                liveness_metrics::TIMEOUT_SOURCE_IDLE_STALL,
                liveness_metrics::FAILURE_CLASS_IDLE_STALL,
                false,
            );
        });
        assert_sync_unit(|| {
            liveness_metrics::record_zero_output_stall(
                std::time::Duration::from_secs(300),
                liveness_metrics::TIMEOUT_SOURCE_FIRST_CALL_HANG,
                liveness_metrics::FAILURE_CLASS_FIRST_CALL_HANG,
                true,
            );
        });
    }

    #[test]
    fn zero_output_stall_histogram_renders_after_recording() {
        let _guard = test_guard();
        init().unwrap();

        liveness_metrics::record_zero_output_stall(
            std::time::Duration::from_secs(120),
            liveness_metrics::TIMEOUT_SOURCE_IDLE_STALL,
            liveness_metrics::FAILURE_CLASS_IDLE_STALL,
            false,
        );
        liveness_metrics::record_zero_output_stall(
            std::time::Duration::from_secs(300),
            liveness_metrics::TIMEOUT_SOURCE_FIRST_CALL_HANG,
            liveness_metrics::FAILURE_CLASS_FIRST_CALL_HANG,
            true,
        );

        let rendered = render().unwrap();
        assert!(
            rendered.contains(ZERO_OUTPUT_STALL_SECONDS),
            "missing {ZERO_OUTPUT_STALL_SECONDS} in rendered output:\n{rendered}",
        );
        assert!(
            rendered.contains(&format!("# HELP {ZERO_OUTPUT_STALL_SECONDS}")),
            "missing HELP line for zero-output stall histogram:\n{rendered}",
        );
        assert!(
            rendered.contains(&format!("# TYPE {ZERO_OUTPUT_STALL_SECONDS} summary")),
            "missing TYPE line for zero-output stall histogram:\n{rendered}",
        );
    }

    #[test]
    fn zero_output_stall_labels_render_with_bounded_dimensions() {
        let _guard = test_guard();
        init().unwrap();

        liveness_metrics::record_zero_output_stall(
            std::time::Duration::from_secs(90),
            liveness_metrics::TIMEOUT_SOURCE_FIRST_CALL_HANG,
            liveness_metrics::FAILURE_CLASS_FIRST_CALL_HANG,
            false,
        );
        liveness_metrics::record_zero_output_stall(
            std::time::Duration::from_secs(600),
            liveness_metrics::TIMEOUT_SOURCE_IDLE_STALL,
            liveness_metrics::FAILURE_CLASS_IDLE_STALL,
            true,
        );

        let rendered = render().unwrap();
        // Both label combos should be present.
        assert!(
            rendered.contains("timeout_source=\"first_call_hang\""),
            "missing first_call_hang timeout_source label:\n{rendered}",
        );
        assert!(
            rendered.contains("failure_class=\"idle_stall\""),
            "missing idle_stall failure_class label:\n{rendered}",
        );
        assert!(
            rendered.contains("chain_exhausted=\"true\""),
            "missing chain_exhausted=true label:\n{rendered}",
        );
        assert!(
            rendered.contains("chain_exhausted=\"false\""),
            "missing chain_exhausted=false label:\n{rendered}",
        );
        // No high-cardinality labels.
        for forbidden in ["task_id=", "session_id=", "provider_id=", "model_id="] {
            for line in rendered.lines() {
                if !line.starts_with(ZERO_OUTPUT_STALL_SECONDS) {
                    continue;
                }
                assert!(
                    !line.contains(forbidden),
                    "zero-output stall metric must not carry high-cardinality label {forbidden}: {line}",
                );
            }
        }
    }

    // ─── prompt-context latency observability tests ───────────────

    #[test]
    fn prompt_context_latency_metric_facade_helpers_are_synchronous_unit_functions() {
        let _guard = test_guard();

        fn assert_sync_unit<F: FnOnce()>(f: F) {
            f();
        }

        init().unwrap();
        assert_sync_unit(|| {
            prompt_context_metrics::record_total(std::time::Duration::from_millis(250));
        });
        for span in prompt_context_metrics::ALL_SPANS {
            assert_sync_unit(|| {
                prompt_context_metrics::record_child_span(
                    span,
                    std::time::Duration::from_millis(50),
                );
            });
        }
    }

    #[test]
    fn prompt_context_total_latency_histogram_renders_after_recording() {
        let _guard = test_guard();
        init().unwrap();

        prompt_context_metrics::record_total(std::time::Duration::from_millis(500));
        prompt_context_metrics::record_total(std::time::Duration::from_millis(1200));

        let rendered = render().unwrap();
        assert!(
            rendered.contains(PROMPT_CONTEXT_LATENCY_SECONDS),
            "missing {PROMPT_CONTEXT_LATENCY_SECONDS} in rendered output:\n{rendered}",
        );
        assert!(
            rendered.contains(&format!("# HELP {PROMPT_CONTEXT_LATENCY_SECONDS}")),
            "missing HELP line for prompt-context total latency:\n{rendered}",
        );
        assert!(
            rendered.contains(&format!("# TYPE {PROMPT_CONTEXT_LATENCY_SECONDS} summary")),
            "missing TYPE line for prompt-context total latency:\n{rendered}",
        );
    }

    #[test]
    fn prompt_context_child_span_latency_renders_span_label() {
        let _guard = test_guard();
        init().unwrap();

        prompt_context_metrics::record_child_span(
            prompt_context_metrics::SPAN_ACTIVITY_DB,
            std::time::Duration::from_millis(100),
        );
        prompt_context_metrics::record_child_span(
            prompt_context_metrics::SPAN_CODE_GRAPH,
            std::time::Duration::from_millis(300),
        );

        let rendered = render().unwrap();
        assert!(
            rendered.contains(PROMPT_CONTEXT_CHILD_SPAN_LATENCY_SECONDS),
            "missing {PROMPT_CONTEXT_CHILD_SPAN_LATENCY_SECONDS} in rendered output:\n{rendered}",
        );
        assert!(
            rendered.contains("span=\"activity_db\""),
            "missing activity_db span label:\n{rendered}",
        );
        assert!(
            rendered.contains("span=\"code_graph\""),
            "missing code_graph span label:\n{rendered}",
        );
        // No high-cardinality labels.
        for forbidden in ["task_id=", "session_id="] {
            for line in rendered.lines() {
                if !line.starts_with(PROMPT_CONTEXT_CHILD_SPAN_LATENCY_SECONDS) {
                    continue;
                }
                assert!(
                    !line.contains(forbidden),
                    "prompt-context child-span metric must not carry high-cardinality label {forbidden}: {line}",
                );
            }
        }
    }

    #[test]
    fn prompt_context_all_span_labels_render_distinctly() {
        let _guard = test_guard();
        init().unwrap();

        for span in prompt_context_metrics::ALL_SPANS {
            prompt_context_metrics::record_child_span(span, std::time::Duration::from_millis(50));
        }

        let rendered = render().unwrap();
        for span in prompt_context_metrics::ALL_SPANS {
            assert!(
                rendered.contains(&format!("span=\"{span}\"")),
                "missing span label {span} in rendered output:\n{rendered}",
            );
        }
    }

    // ── Rollout-validation counter tests (proposal uk2d AC17) ─────────

    #[test]
    fn infra_exempt_registered_labels_render_at_zero_on_init() {
        let _guard = test_guard();
        init().unwrap();
        let rendered = render().unwrap();
        for outcome in infra_delta::OUTCOMES {
            for is_infra in ["true", "false"] {
                rendered_sample(
                    &rendered,
                    INFRA_EXEMPT_TOTAL,
                    &[("outcome", outcome), ("is_infra", is_infra)],
                );
            }
        }
    }

    #[test]
    fn infra_exempt_counter_increments_and_renders() {
        let _guard = test_guard();
        init().unwrap();

        infra_delta::increment(infra_delta::OUTCOME_TOTAL, true);
        infra_delta::increment(infra_delta::OUTCOME_TOTAL, true);
        infra_delta::increment(infra_delta::OUTCOME_QUALITY_STRIKE, false);

        let rendered = render().unwrap();
        assert!(
            labeled_sample_value(
                &rendered,
                INFRA_EXEMPT_TOTAL,
                &[("outcome", "total"), ("is_infra", "true")]
            ) >= 2.0,
            "infra_exempt total/true should be >= 2"
        );
        assert!(
            labeled_sample_value(
                &rendered,
                INFRA_EXEMPT_TOTAL,
                &[("outcome", "quality_strike"), ("is_infra", "false")]
            ) >= 1.0,
            "infra_exempt quality_strike/false should be >= 1"
        );
    }

    #[test]
    fn fallback_rescue_counter_increments_and_renders() {
        let _guard = test_guard();
        init().unwrap();

        fallback_rescue::increment_rescue();

        let rendered = render().unwrap();
        assert!(
            rendered.contains(FALLBACK_RESCUE_TOTAL),
            "missing {FALLBACK_RESCUE_TOTAL} in rendered output:\n{rendered}"
        );
        assert!(
            unlabelled_sample_value(&rendered, FALLBACK_RESCUE_TOTAL) >= 1.0,
            "fallback rescue should increment by 1"
        );
    }

    #[test]
    fn reasoning_kill_registered_labels_render_at_zero_on_init() {
        let _guard = test_guard();
        init().unwrap();
        let rendered = render().unwrap();
        for fc in reasoning_kill::FAILURE_CLASSES {
            for mc in reasoning_kill::MODEL_CONTEXTS {
                for oc in reasoning_kill::OUTCOMES {
                    rendered_sample(
                        &rendered,
                        REASONING_KILL_TOTAL,
                        &[
                            ("model_context", mc),
                            ("failure_class", fc),
                            ("outcome", oc),
                        ],
                    );
                }
            }
        }
    }

    #[test]
    fn reasoning_kill_counter_increments_and_renders() {
        let _guard = test_guard();
        init().unwrap();

        reasoning_kill::increment(
            reasoning_kill::MODEL_CONTEXT_REASONING,
            reasoning_kill::FAILURE_CLASS_FIRST_CALL_HANG,
            reasoning_kill::OUTCOME_KILLED,
        );
        reasoning_kill::increment(
            reasoning_kill::MODEL_CONTEXT_NON_REASONING,
            reasoning_kill::FAILURE_CLASS_IDLE_STALL,
            reasoning_kill::OUTCOME_RESCUED,
        );
        reasoning_kill::increment(
            reasoning_kill::MODEL_CONTEXT_REASONING,
            reasoning_kill::FAILURE_CLASS_IDLE_STALL,
            reasoning_kill::OUTCOME_TYPED_FAILURE,
        );

        let rendered = render().unwrap();
        assert!(
            labeled_sample_value(
                &rendered,
                REASONING_KILL_TOTAL,
                &[
                    ("model_context", "reasoning"),
                    ("failure_class", "first_call_hang"),
                    ("outcome", "killed"),
                ]
            ) >= 1.0,
            "reasoning kill reasoning/first_call_hang/killed should be >= 1"
        );
        assert!(
            labeled_sample_value(
                &rendered,
                REASONING_KILL_TOTAL,
                &[
                    ("model_context", "non_reasoning"),
                    ("failure_class", "idle_stall"),
                    ("outcome", "rescued"),
                ]
            ) >= 1.0,
            "reasoning kill non_reasoning/idle_stall/rescued should be >= 1"
        );
        assert!(
            labeled_sample_value(
                &rendered,
                REASONING_KILL_TOTAL,
                &[
                    ("model_context", "reasoning"),
                    ("failure_class", "idle_stall"),
                    ("outcome", "typed_failure"),
                ]
            ) >= 1.0,
            "reasoning kill reasoning/idle_stall/typed_failure should be >= 1"
        );
    }

    #[test]
    fn reasoning_kill_is_reasoning_model_heuristic() {
        // Known reasoning model patterns.
        assert!(reasoning_kill::is_reasoning_model("openai/o1-mini"));
        assert!(reasoning_kill::is_reasoning_model("openai/o1-preview"));
        assert!(reasoning_kill::is_reasoning_model("openai/o3-mini"));
        assert!(reasoning_kill::is_reasoning_model(
            "xiaomi-token-plan-sgp/mimo-v2.5-pro"
        ));
        assert!(reasoning_kill::is_reasoning_model("zai/GLM-5"));
        assert!(reasoning_kill::is_reasoning_model(
            "moonshotai/kimi-k2-thinking"
        ));
        assert!(reasoning_kill::is_reasoning_model(
            "custom/some-thinking-model"
        ));

        // Non-reasoning models.
        assert!(!reasoning_kill::is_reasoning_model("openai/gpt-4o"));
        assert!(!reasoning_kill::is_reasoning_model(
            "anthropic/claude-3.5-sonnet"
        ));
        assert!(!reasoning_kill::is_reasoning_model("openai/gpt-4-turbo"));

        // Bare model id (no provider prefix).
        assert!(reasoning_kill::is_reasoning_model("o1-mini"));
        assert!(!reasoning_kill::is_reasoning_model("gpt-4o"));
    }

    #[test]
    fn reasoning_kill_outcomes_render_as_distinct_samples() {
        let _guard = test_guard();
        init().unwrap();

        // Emit one of each outcome for the same model_context/failure_class.
        reasoning_kill::increment(
            reasoning_kill::MODEL_CONTEXT_REASONING,
            reasoning_kill::FAILURE_CLASS_FIRST_CALL_HANG,
            reasoning_kill::OUTCOME_KILLED,
        );
        reasoning_kill::increment(
            reasoning_kill::MODEL_CONTEXT_REASONING,
            reasoning_kill::FAILURE_CLASS_FIRST_CALL_HANG,
            reasoning_kill::OUTCOME_RESCUED,
        );
        reasoning_kill::increment(
            reasoning_kill::MODEL_CONTEXT_REASONING,
            reasoning_kill::FAILURE_CLASS_FIRST_CALL_HANG,
            reasoning_kill::OUTCOME_TYPED_FAILURE,
        );

        let rendered = render().unwrap();
        for oc in reasoning_kill::OUTCOMES {
            let val = labeled_sample_value(
                &rendered,
                REASONING_KILL_TOTAL,
                &[
                    ("model_context", "reasoning"),
                    ("failure_class", "first_call_hang"),
                    ("outcome", oc),
                ],
            );
            assert!(val >= 1.0, "reasoning kill outcome={oc} should be >= 1");
        }
    }

    #[test]
    fn rollout_counters_do_not_contain_high_cardinality_labels() {
        let _guard = test_guard();
        init().unwrap();

        infra_delta::increment(infra_delta::OUTCOME_TOTAL, true);
        fallback_rescue::increment_rescue();
        reasoning_kill::increment(
            reasoning_kill::MODEL_CONTEXT_REASONING,
            reasoning_kill::FAILURE_CLASS_FIRST_CALL_HANG,
            reasoning_kill::OUTCOME_KILLED,
        );

        let rendered = render().unwrap();
        for forbidden in ["task_id=", "session_id=", "attempt_id=", "session_idx="] {
            for line in rendered.lines() {
                if !line.starts_with(INFRA_EXEMPT_TOTAL)
                    && !line.starts_with(FALLBACK_RESCUE_TOTAL)
                    && !line.starts_with(REASONING_KILL_TOTAL)
                {
                    continue;
                }
                assert!(
                    !line.contains(forbidden),
                    "rollout-validation metric must not carry high-cardinality label {forbidden}: {line}",
                );
            }
        }
    }

    // ── Arbiter rollout metrics tests ──────────────────────────────────

    #[test]
    fn arbiter_decision_metric_names_and_labels_render() {
        let _guard = test_guard();
        init().unwrap();

        // Record one of each decision type.
        for decision in arbiter::ALL_DECISIONS {
            arbiter::record_decision(decision);
        }

        let rendered = render().unwrap();
        for decision in arbiter::ALL_DECISIONS {
            rendered_sample(&rendered, ARBITER_DECISION_TOTAL, &[("decision", decision)]);
        }
    }

    #[test]
    fn arbiter_park_metric_names_and_labels_render() {
        let _guard = test_guard();
        init().unwrap();

        arbiter::record_park(
            arbiter::PARK_REASON_DEADLINE_EXPIRED,
            arbiter::PARK_OUTCOME_SUCCESS,
        );
        arbiter::record_park(
            arbiter::PARK_REASON_CONSUMED_REENTRY,
            arbiter::PARK_OUTCOME_TRANSITION_FAILED,
        );
        arbiter::record_park(
            arbiter::PARK_REASON_ARBITER_DECIDED,
            arbiter::PARK_OUTCOME_RECOVERY,
        );

        let rendered = render().unwrap();
        rendered_sample(
            &rendered,
            ARBITER_PARK_TOTAL,
            &[
                ("reason", arbiter::PARK_REASON_DEADLINE_EXPIRED),
                ("outcome", arbiter::PARK_OUTCOME_SUCCESS),
            ],
        );
        rendered_sample(
            &rendered,
            ARBITER_PARK_TOTAL,
            &[
                ("reason", arbiter::PARK_REASON_CONSUMED_REENTRY),
                ("outcome", arbiter::PARK_OUTCOME_TRANSITION_FAILED),
            ],
        );
        rendered_sample(
            &rendered,
            ARBITER_PARK_TOTAL,
            &[
                ("reason", arbiter::PARK_REASON_ARBITER_DECIDED),
                ("outcome", arbiter::PARK_OUTCOME_RECOVERY),
            ],
        );
    }

    #[test]
    fn arbiter_monitored_reopen_metric_names_and_labels_render() {
        let _guard = test_guard();
        init().unwrap();

        for outcome in arbiter::ALL_REOPEN_OUTCOMES {
            arbiter::record_monitored_reopen(outcome);
        }

        let rendered = render().unwrap();
        for outcome in arbiter::ALL_REOPEN_OUTCOMES {
            rendered_sample(
                &rendered,
                ARBITER_MONITORED_REOPEN_TOTAL,
                &[("outcome", outcome)],
            );
        }
    }

    #[test]
    fn arbiter_termination_metric_names_and_labels_render() {
        let _guard = test_guard();
        init().unwrap();

        for class in arbiter::ALL_TERMINATION_CLASSES {
            arbiter::record_termination(class);
        }

        let rendered = render().unwrap();
        for class in arbiter::ALL_TERMINATION_CLASSES {
            rendered_sample(&rendered, ARBITER_TERMINATION_TOTAL, &[("class", class)]);
        }
    }

    #[test]
    fn arbiter_time_in_arbitration_histogram_renders() {
        let _guard = test_guard();
        init().unwrap();

        arbiter::record_time_in_arbitration(42.5);
        arbiter::record_time_in_arbitration(100.0);

        let rendered = render().unwrap();
        assert!(
            rendered.contains(ARBITER_TIME_IN_ARBITRATION_SECONDS),
            "time-in-arbitration histogram must appear in rendered output:\\n{rendered}"
        );
    }

    #[test]
    fn arbiter_metrics_do_not_contain_high_cardinality_labels() {
        let _guard = test_guard();
        init().unwrap();

        arbiter::record_decision(arbiter::DECISION_APPROVE);
        arbiter::record_park(
            arbiter::PARK_REASON_DEADLINE_EXPIRED,
            arbiter::PARK_OUTCOME_SUCCESS,
        );
        arbiter::record_monitored_reopen(arbiter::REOPEN_OUTCOME_STARTED);
        arbiter::record_termination(arbiter::TERMINATION_INFRA);
        arbiter::record_time_in_arbitration(10.0);

        let rendered = render().unwrap();
        for forbidden in ["task_id=", "session_id=", "attempt_id=", "hold_cycle="] {
            for line in rendered.lines() {
                if !line.starts_with("djinn_arbiter_") {
                    continue;
                }
                assert!(
                    !line.contains(forbidden),
                    "arbiter metric must not carry high-cardinality label {forbidden}: {line}",
                );
            }
        }
    }
}

// warm-base validation: v0.6.11 incremental=0 (no-op)
