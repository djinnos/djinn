//! The Kueue cutover's telemetry gate (`plcj`, acceptance criterion 3).
//!
//! # What is being asserted, and why not a grep
//!
//! The cutover deleted the pre-create build-admission ledger. Ten gauges and
//! counters that only that authority ever wrote went with it — see the comment
//! block above [`super::BUILD_ADMISSION_SHADOW_INVOCATION_TOTAL`]. The failure
//! mode this file guards is NOT "a stale constant survived in the source": it is
//! **a series that still reaches Prometheus while the relation behind it is
//! gone**. A described-but-unwritten series renders as permanently absent, which
//! a dashboard reads as *healthy*.
//!
//! A `grep` over `src/` cannot tell those two apart. It fires on a comment that
//! merely names the dead relation (this crate has several, deliberately), and it
//! stays silent on a metric assembled at runtime. So every assertion here is
//! made against **a live Prometheus registry**, on the names that registry
//! actually rendered after the crate's own emission surface was driven through
//! it.
//!
//! # Why an isolated registry
//!
//! The recorder installed by [`super::init`] is process-global and cargo runs
//! every test in a binary as one process, so a global-registry assertion would
//! see whatever the crate's other ~40 tests emitted, in whatever order they
//! happened to run. PR #2824 added [`super::IsolatedRecorder`] for exactly this
//! flake class; it is used here rather than [`super::render`].
//!
//! # The three tests, and what each would miss alone
//!
//! 1. [`no_emitted_metric_names_a_relation_the_kueue_cutover_deleted`] drives
//!    the crate's whole public emission surface into a private registry and
//!    asserts no rendered name carries a retired token. Alone, it would pass if
//!    the sweep silently stopped covering the crate.
//! 2. [`the_emission_sweep_covers_every_metric_the_crate_describes`] closes
//!    that: every metric name `register_metrics` declares — captured live, from
//!    a recorder that intercepts the `describe_*` calls — must appear among the
//!    names the sweep rendered. A metric added without a sweep entry fails here
//!    and names itself.
//! 3. [`the_retired_relation_detector_flags_a_planted_series`] proves the
//!    detector has teeth: the same predicate, over a registry that DID emit
//!    `djinn_build_admission_journal_degraded`, must report it. Without this a
//!    typo in [`RETIRED_RELATION_TOKENS`] would make tests 1 and 2 vacuous.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};

use super::{IsolatedRecorder, register_metrics};

/// Substrings that may not appear in any metric name this crate can emit.
///
/// Every entry is a relation or authority the Kueue cutover removed, not a
/// stylistic preference:
///
/// * `admission_journal` — the pods-quota reservation ledger (migration 121).
///   Its repository was deleted by `o53p`; nothing reads it.
/// * `generation_ack` — `admission_handoff_generation_ack` (migration 149),
///   retired with the v0→v1 handoff by `ubne`.
/// * `admission_inventory`, `admission_transition`, `admission_occupancy`,
///   `admission_stale`, `handoff_warning`, `seconds_since_reconcile`,
///   `create_unknown_health`, `would_defer`, `unknown_classification` — the
///   metric families written only by the deleted `BuildAdmissionController` and
///   its reconciler.
///
/// `admission_handoff` itself is deliberately ABSENT from this list. `ubne`
/// retired the handoff *semantics* but kept the physical row as the invocation
/// lease's arming authority (`InvocationLeaseAuthorityRepository`), so it is a
/// live relation, and banning its name would be a false positive.
const RETIRED_RELATION_TOKENS: &[&str] = &[
    "admission_journal",
    "generation_ack",
    "admission_inventory",
    "admission_transition",
    "admission_occupancy",
    "admission_stale",
    "handoff_warning",
    "seconds_since_reconcile",
    "create_unknown_health",
    "would_defer",
    "unknown_classification",
];

/// Metric names in `rendered` that carry a retired token.
///
/// Shared by the assertion and by its own neutralisation test, so the two can
/// never drift apart.
fn retired_offenders(names: &BTreeSet<String>) -> Vec<String> {
    names
        .iter()
        .filter(|name| {
            RETIRED_RELATION_TOKENS
                .iter()
                .any(|token| name.contains(token))
        })
        .cloned()
        .collect()
}

/// Metric names a Prometheus render declared, read off its `# TYPE` lines.
///
/// The `# TYPE` line carries the base name for every metric kind, including
/// histograms whose samples are suffixed `_bucket` / `_sum` / `_count`, so this
/// is the complete set of *names* the registry produced — which is exactly what
/// the acceptance criterion is about.
fn rendered_metric_names(rendered: &str) -> BTreeSet<String> {
    rendered
        .lines()
        .filter_map(|line| line.strip_prefix("# TYPE "))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(str::to_owned)
        .collect()
}

/// A recorder that answers every registration with a no-op handle and only
/// remembers the names it was asked about.
///
/// This is how the described-metric inventory is obtained from the live
/// `metrics` path rather than by reading the source: `register_metrics` is run
/// against it and every `describe_*` key it emits is captured.
#[derive(Default)]
struct NameCapturingRecorder {
    names: Mutex<BTreeSet<String>>,
}

impl NameCapturingRecorder {
    fn capture(&self, key: &str) {
        self.names
            .lock()
            .expect("name capture mutex poisoned")
            .insert(key.to_owned());
    }

    fn names(&self) -> BTreeSet<String> {
        self.names
            .lock()
            .expect("name capture mutex poisoned")
            .clone()
    }
}

impl Recorder for NameCapturingRecorder {
    fn describe_counter(&self, key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        self.capture(key.as_str());
    }

    fn describe_gauge(&self, key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        self.capture(key.as_str());
    }

    fn describe_histogram(&self, key: KeyName, _unit: Option<Unit>, _description: SharedString) {
        self.capture(key.as_str());
    }

    fn register_counter(&self, key: &Key, _metadata: &Metadata<'_>) -> Counter {
        self.capture(key.name());
        Counter::noop()
    }

    fn register_gauge(&self, key: &Key, _metadata: &Metadata<'_>) -> Gauge {
        self.capture(key.name());
        Gauge::noop()
    }

    fn register_histogram(&self, key: &Key, _metadata: &Metadata<'_>) -> Histogram {
        self.capture(key.name());
        Histogram::noop()
    }
}

/// Drive every public emission entry point this crate exposes exactly once.
///
/// Label VALUES are deliberately arbitrary where the signature accepts a
/// `&str`: labels do not participate in a metric's name, and the closed-label
/// contracts are already covered by the per-family tests in `lib.rs`. What
/// matters here is that every name the crate can produce is produced.
///
/// Calls are grouped and ordered to match the module order in `lib.rs`, so a
/// reviewer can walk the two side by side.
/// `super::render_isolated`'s soundness rule applies: this is a synchronous
/// body and every call records on the calling thread.
#[allow(clippy::too_many_lines)]
fn emit_every_public_metric() {
    use super::{
        agent_session_phase, arbiter, board_health_mismatch, breaker, build_admission,
        build_slot_occupancy, build_slot_queue, cache_cleanup, canonical_graph_slot, cargo_cache,
        cargo_invocation, cargo_target_seed, cargo_warm_base, cargo_warm_incremental_prune,
        cargo_warm_step, dispatch, doctor, failover, fallback_rescue, galaxy_artifact_publication,
        galaxy_artifact_route, graph_retention, infra_delta, inline_cleanup, jit_pitfalls,
        liveness_metrics, memory_retrieval, pr_poller, preservation, prompt_context_metrics, psi,
        reasoning_kill, refinement_run, reply_loop, role_class, run_dir, server_memory, slot_pool,
        stale_sweep, task, taskrun_lifecycle, warm_cache, workspace_cleanup, workspace_clone,
        workspace_seed, zombie,
    };

    let tick = Duration::from_millis(1);

    taskrun_lifecycle::increment_job_started();
    taskrun_lifecycle::increment_worker_completion_submitted();

    breaker::increment_trip();
    breaker::set_state("global", "model", 1.0);

    zombie::increment_reap(zombie::KIND_STARTUP);
    refinement_run::increment_reaped_phantom();
    jit_pitfalls::increment_outcome(jit_pitfalls::OUTCOME_INJECTED);

    server_memory::record_process_rss(2, 1);
    server_memory::record_limit_bytes(4);
    server_memory::record_process_unavailable();
    server_memory::record_jemalloc_stats(3, 2, 1);
    server_memory::record_jemalloc_unavailable();

    task::increment_reopen();
    task::increment_parked();
    task::increment_parked_labeled(1, 0, 0, 1);

    pr_poller::set_tracked(1);
    pr_poller::increment_merge_failure();

    inline_cleanup::increment_pr_closed();
    inline_cleanup::increment_branch_deleted();
    inline_cleanup::increment_skipped(inline_cleanup::REASON_DRY_RUN);

    board_health_mismatch::record_page(1);
    board_health_mismatch::record_duration(tick);
    board_health_mismatch::record_coalesced("tick");
    board_health_mismatch::record_outcome("ok", "tick");
    board_health_mismatch::record_pass_age(Some("2026-07-30T00:00:00.000Z"));

    doctor::set_findings("check", 1);
    doctor::set_run_duration_seconds("check", 1.0);
    doctor::record_run_duration("check", tick);
    doctor::record_retrieval_refresh("ok", tick, 1.0);

    cargo_cache::record_seed_hit("project");
    cargo_cache::record_seed_cold("project", "base_missing");
    cargo_cache::record_warm_base_freshness("project", 1.0);
    cargo_cache::record_warm_step_fresh_count("project", "clippy", 1);
    cargo_cache::record_warm_step_compiling_count("project", "clippy", 1);

    stale_sweep::increment_pr_reaped();
    stale_sweep::increment_branch_reaped();
    stale_sweep::increment_pr_skipped(stale_sweep::REASON_API_ERROR);
    stale_sweep::increment_orphan_session_reaped();

    cargo_warm_step::record_step_seconds(
        "project",
        cargo_warm_step::STEP_CLIPPY,
        cargo_warm_step::OUTCOME_OK,
        1.0,
    );
    cargo_warm_step::increment_step(
        "project",
        cargo_warm_step::STEP_CLIPPY,
        cargo_warm_step::OUTCOME_OK,
    );
    cargo_warm_step::set_workspace_path("project", "/workspace");

    cargo_warm_base::set_units(
        "project",
        cargo_warm_base::PHASE_PRE_COMPILE,
        cargo_warm_base::KIND_DEP_FILES,
        1,
    );
    cargo_warm_base::increment_sweep_decision("project", cargo_warm_base::DECISION_SWEPT);

    cargo_warm_incremental_prune::increment_attempt(
        "project",
        cargo_warm_incremental_prune::Outcome::Pruned,
    );
    cargo_warm_incremental_prune::add_pruned_bytes("project", 1);

    cargo_target_seed::increment_seed_hit();
    cargo_target_seed::increment_seed_fallback(cargo_target_seed::FALLBACK_REASON_UNKNOWN);
    cargo_target_seed::add_seed_entries(cargo_target_seed::DISPOSITION_UNSEEDED, 1);

    warm_cache::record_decision(
        "project",
        warm_cache::Outcome::Hit,
        warm_cache::Reason::Fresh,
    );

    dispatch::increment_strike_decision(
        dispatch::STRIKE_DECISION_COUNTED,
        dispatch::STRIKE_SOURCE_CRASHED,
    );
    dispatch::increment_attempt(dispatch::OUTCOME_OK);
    dispatch::record_cross_model_review("approved");
    dispatch::record_last_success_timestamp(1.0);
    dispatch::set_cooldowns_active(1);
    dispatch::set_inflight_ledger_size(1);
    dispatch::set_user_cap_utilization("user", "model", 1, 2);
    dispatch::record_success_at(1.0);
    dispatch::increment_ok();
    dispatch::increment_cooldown();
    dispatch::increment_cap();
    dispatch::increment_breaker();
    dispatch::increment_error();

    slot_pool::set_slots(slot_pool::STATE_FREE, "model", 1);

    preservation::increment_attempt(preservation::OUTCOME_SUCCEEDED, preservation::TRIGGER_STALL);

    failover::increment_candidate_attempt("ok", "provider", "model");
    failover::increment_candidate_accepted("provider", "model");
    failover::increment_chain_exhausted("provider", "model");
    failover::record_latency(tick);

    liveness_metrics::record_zero_output_stall(tick, "idle", "stall", false);

    prompt_context_metrics::record_total(tick);
    prompt_context_metrics::record_child_span("knowledge", tick);

    infra_delta::increment("ok", true);
    fallback_rescue::increment_rescue();
    reply_loop::increment_inline_char_budget_trip();
    reasoning_kill::increment("reasoning", "idle_stall", "killed");

    arbiter::record_decision("approve");
    arbiter::record_park("stall", "parked");
    arbiter::record_monitored_reopen("reopened");
    arbiter::record_termination("clean");
    arbiter::record_time_in_arbitration(1.0);

    cache_cleanup::increment_pressure_unit(
        cache_cleanup::PressureMode::DryRun,
        cache_cleanup::PressureRung::Base,
        cache_cleanup::PressureOutcome::Planned,
    );
    cache_cleanup::record_pressure_projected_allocated_bytes(
        cache_cleanup::PressureMode::DryRun,
        cache_cleanup::PressureRung::Base,
        1,
    );
    cache_cleanup::record_pressure_reclaimed_allocated_bytes(
        cache_cleanup::PressureMode::DryRun,
        cache_cleanup::PressureRung::Base,
        1,
    );
    cache_cleanup::increment_pressure_termination(
        cache_cleanup::PressureMode::DryRun,
        cache_cleanup::PressureTermination::Completed,
    );
    cache_cleanup::increment_cleanup_total(
        cache_cleanup::COMPONENT_SCCACHE,
        cache_cleanup::OUTCOME_DELETED,
        "dry_run",
    );
    cache_cleanup::record_reclaimed_bytes(cache_cleanup::COMPONENT_SCCACHE, "dry_run", 1);
    cache_cleanup::increment_candidates(cache_cleanup::COMPONENT_SCCACHE, "dry_run", 1);

    // Emits nothing; called so the sweep visibly covers the module rather than
    // leaving a reader to wonder whether it was forgotten.
    role_class::observe(role_class::CLASS_LIGHT);

    cargo_invocation::record_seconds("build", "ok", "warm", tick);
    workspace_clone::record_seconds("ok", tick);
    workspace_seed::record_seconds("ok", tick);
    workspace_cleanup::record_seconds("cancel", "ok", tick);

    build_slot_queue::record_wait_seconds(build_slot_queue::OUTCOME_ADMITTED, tick);
    build_slot_occupancy::set_slots_in_use(1);
    build_slot_occupancy::set_slots_queued(1);

    // The surviving per-invocation cgroup-lease family. Everything the deleted
    // pre-create ledger wrote used to live in this same module, which is why it
    // is the one whose rendered names matter most here.
    build_admission::record_shadow_invocation(true);
    build_admission::record_lift_rejected();
    build_admission::record_invocation_degraded("cancelled");

    run_dir::set_state_count("active", 1);
    run_dir::set_state_allocated_bytes("active", 1);
    run_dir::set_reserved_bytes(1);
    run_dir::set_unowned_bytes(1);
    run_dir::increment_queue_reason("insufficient_disk");
    run_dir::increment_quota_failure("setquota_failed");
    run_dir::increment_reclaim("released", 1, 1);
    run_dir::increment_seed_outcome("hit");
    run_dir::increment_warm_base_removed();

    agent_session_phase::add_phase_duration("provider_wait", "worker", tick);

    psi::record_success("cpu", 1.0);
    psi::record_failure("cpu", "unavailable");

    graph_retention::increment("delete", "ok", "aged_out", 1);

    canonical_graph_slot::record_install(canonical_graph_slot::Source::Warm, Some(1), 1, 1);
    canonical_graph_slot::initialize_empty();
    canonical_graph_slot::record_cleared();

    galaxy_artifact_publication::record_build_duration(tick);
    galaxy_artifact_publication::record_publication_duration(tick);
    galaxy_artifact_publication::record_sizes(11, 7, 1);
    galaxy_artifact_publication::record_success();
    galaxy_artifact_publication::record_oversize();
    galaxy_artifact_publication::record_failure();

    galaxy_artifact_route::record_outcome("ok");
    galaxy_artifact_route::record_chunk(17);

    let retrieval = memory_retrieval::MemoryRetrievalMetrics::new();
    retrieval
        .observe(
            memory_retrieval::RetrievalEntryPoint::Dispatch,
            memory_retrieval::RetrievalOutcome::Success,
            tick,
            1,
        )
        .expect("retrieval observation");
    retrieval
        .observe_stage(
            memory_retrieval::RetrievalEntryPoint::Dispatch,
            memory_retrieval::RetrievalStage::Lexical,
            tick,
        )
        .expect("retrieval stage observation");
}

/// Render the sweep into a registry no other test can reach.
fn sweep_rendered_names() -> BTreeSet<String> {
    let recorder = IsolatedRecorder::new();
    {
        let _guard = recorder.scope();
        emit_every_public_metric();
    }
    rendered_metric_names(&recorder.render())
}

/// Every distinct metric name [`emit_every_public_metric`] currently produces.
///
/// Pinned, not bounded — see the assertion that uses it.
const EMITTED_METRIC_NAME_COUNT: usize = 128;

/// **The acceptance criterion.** No metric this crate can emit names a relation
/// the Kueue cutover deleted — asserted on the names a live registry rendered.
#[test]
fn no_emitted_metric_names_a_relation_the_kueue_cutover_deleted() {
    let names = sweep_rendered_names();

    // A sweep that stopped emitting would satisfy the offender check
    // vacuously, so the count is pinned rather than bounded. An exact number is
    // the only form that also catches a call being REMOVED from
    // `emit_every_public_metric`, which a floor would silently absorb.
    assert_eq!(
        names.len(),
        EMITTED_METRIC_NAME_COUNT,
        "the emission sweep rendered {} metric names instead of {}. If you \
         added a metric, add a call to `emit_every_public_metric` and bump \
         this number; if you removed one, bump it down. Do NOT relax this into \
         an inequality — it is what stops the sweep from quietly narrowing \
         until the assertion below covers nothing.",
        names.len(),
        EMITTED_METRIC_NAME_COUNT,
    );

    let offenders = retired_offenders(&names);
    assert!(
        offenders.is_empty(),
        "these emitted metrics name a relation the Kueue cutover deleted: {offenders:?}"
    );
}

/// The sweep must keep up with the crate.
///
/// Every name `register_metrics` describes is captured from the live `metrics`
/// recorder path — not read out of the source — and must appear among the names
/// the sweep actually rendered. A metric added without a matching sweep entry
/// fails here and names itself, which is what stops
/// [`no_emitted_metric_names_a_relation_the_kueue_cutover_deleted`] from
/// quietly narrowing over time.
#[test]
fn the_emission_sweep_covers_every_metric_the_crate_describes() {
    let capture = NameCapturingRecorder::default();
    metrics::with_local_recorder(&capture, register_metrics);
    let described = capture.names();
    assert!(
        described.len() >= 80,
        "register_metrics described only {} metrics; the coverage check below \
         would be vacuous",
        described.len()
    );

    let emitted = sweep_rendered_names();
    let uncovered: Vec<&String> = described
        .iter()
        .filter(|name| !emitted.contains(*name))
        .collect();
    assert!(
        uncovered.is_empty(),
        "these metrics are described but the emission sweep never emits them, \
         so the retired-relation assertion does not cover them — add a call to \
         `emit_every_public_metric`: {uncovered:?}"
    );
}

/// Neutralisation. The detector must actually detect.
///
/// A registry that DID emit one of the deleted families is run through the same
/// predicate the assertion uses. If [`RETIRED_RELATION_TOKENS`] were misspelled
/// or emptied, the two tests above would pass while proving nothing; this one
/// fails instead.
#[test]
fn the_retired_relation_detector_flags_a_planted_series() {
    let recorder = IsolatedRecorder::new();
    {
        let _guard = recorder.scope();
        // One of the ten gauges deleted with the pre-create admission ledger.
        metrics::gauge!("djinn_build_admission_journal_degraded").set(1.0);
    }
    let names = rendered_metric_names(&recorder.render());
    assert!(
        names.contains("djinn_build_admission_journal_degraded"),
        "the planted series must reach the isolated registry, or this test \
         proves nothing about the detector: {names:?}"
    );
    assert_eq!(
        retired_offenders(&names),
        vec!["djinn_build_admission_journal_degraded".to_owned()],
        "the detector used by the acceptance assertion must flag a retired \
         relation when one is genuinely emitted"
    );
}
