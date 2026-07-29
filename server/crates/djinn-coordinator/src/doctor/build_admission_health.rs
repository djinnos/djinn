//! Operator-facing doctor check for a wedged build-admission controller.
//!
//! ## Why this check exists
//!
//! On 2026-07-29 the whole board stopped dispatching for five hours. The
//! build-admission controller had latched a fail-closed readiness state and
//! nothing in the running process could clear it. What made it FIVE HOURS
//! rather than five minutes was not the latch — it was that no operator-facing
//! surface said so:
//!
//! * `board_health` reported every starved task with `gate_verdict:
//!   "unexplained"` and `reasons: []`, because the dispatch gate it evaluates
//!   knows nothing about admission readiness;
//! * the `stranded_ready` doctor check therefore fired once per VICTIM and
//!   never once about the CAUSE;
//! * the actual reason existed only as a process-local enum on the leader, and
//!   the only way to see it was to grep container logs on the node.
//!
//! This check reports the cause directly, and NAMES the admission-journal rows
//! responsible rather than reporting a count.
//!
//! ## Can this check actually observe the readiness? — yes, and here is why
//!
//! Readiness is process-local in-memory state on the LEADER, so a check that
//! ran on a standby would either see a different controller or none at all.
//! That is not what happens here, and the reason is structural rather than
//! incidental:
//!
//! * every coordinator doctor check is registered from
//!   [`crate::actor::CoordinatorActor::new`];
//! * the actor is constructed only by `AppState::initialize_agents`;
//! * which is called only from `AppState::become_leader`.
//!
//! So the registry that contains this check is, by construction, the LEADER's
//! registry, and the `Arc<BuildAdmissionController>` this check reads is the
//! same handle the leader admits through — [`CoordinatorDeps::build_admission`]
//! is threaded into the actor for exactly that purpose. A standby pod does not
//! register the check at all, so it reports "not registered" rather than
//! silently passing. That distinction matters: an absent check is visible, a
//! check that always passes is not.
//!
//! [`CoordinatorDeps::build_admission`]: crate::types::CoordinatorDeps
//!
//! ## What is NOT covered
//!
//! `create_unknown_pending` is separately derivable from a single SQL count
//! over `admission_journal`, so that one gate could also be observed durably
//! from any pod. The other readiness variants (inventory, topology, journal
//! recovery, over-cap, draining) are process-local with no durable projection,
//! and giving them one would need a heartbeat row and a migration. This check
//! covers all of them for the leader, which is the process whose readiness
//! decides admission; it does not make them observable from a standby.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use djinn_core::clock::{Clock, SystemClock};
use djinn_core::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorResult, Finding, FindingSeverity, ResolverSnapshot,
};
use serde_json::json;

use crate::build_admission::{
    BuildAdmissionController, BuildAdmissionHealthReport, BuildAdmissionMode,
};

pub const BUILD_ADMISSION_HEALTH_CHECK_NAME: &str = "build_admission_health";

/// How long admission may be continuously non-`Healthy` before it is a finding.
///
/// Startup legitimately walks through several non-healthy readiness reasons,
/// and a rolling deploy legitimately parks a row inside the 300s reclaim settle
/// window, so the window has to clear both. Anything past it is a wedge.
pub const DEFAULT_UNHEALTHY_WARN_SECONDS: i64 = 300;
pub const DEFAULT_UNHEALTHY_ERROR_SECONDS: i64 = 900;
pub const DEFAULT_UNHEALTHY_CRITICAL_SECONDS: i64 = 1800;

/// How long the last blocker-free reconciliation pass may be, before the
/// reconciler itself is reported as dead or hung. Comfortably above the 120s
/// default cadence plus one 300s pass budget.
pub const DEFAULT_RECONCILE_STALE_SECONDS: i64 = 900;

/// Reason labels. Bounded and closed so an alert can key on them.
pub const REASON_ADMISSION_WEDGED: &str = "build_admission_not_ready";
pub const REASON_RECONCILE_STALE: &str = "build_admission_reconcile_stale";

/// One sample of leader-local build-admission health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildAdmissionHealthObservation {
    /// The controller's own bounded report.
    pub report: BuildAdmissionHealthReport,
    /// How long readiness has continuously held its current non-healthy value.
    /// Zero while healthy.
    pub unhealthy_for_seconds: i64,
    /// How long this source has been observing. Used to keep a freshly-elected
    /// leader from reporting "no reconciliation has ever completed" before the
    /// first tick could possibly have run.
    pub observed_for_seconds: i64,
}

/// Source of a build-admission health sample.
///
/// Production wraps the leader's live controller; tests provide an in-memory
/// double so the severity and window logic is hermetic.
pub trait BuildAdmissionHealthSource: Send + Sync {
    /// `None` means this process has no admission controller at all (mode Off,
    /// or a runtime with no admission coupling), which is not a finding.
    fn snapshot(&self) -> Option<BuildAdmissionHealthObservation>;
}

fn severity_for_unhealthy(elapsed_seconds: i64) -> FindingSeverity {
    if elapsed_seconds >= DEFAULT_UNHEALTHY_CRITICAL_SECONDS {
        FindingSeverity::Critical
    } else if elapsed_seconds >= DEFAULT_UNHEALTHY_ERROR_SECONDS {
        FindingSeverity::Error
    } else {
        FindingSeverity::Warn
    }
}

/// Cheap, read-only doctor check over leader-local build-admission health.
pub struct BuildAdmissionHealthCheck {
    source: Arc<dyn BuildAdmissionHealthSource>,
}

impl BuildAdmissionHealthCheck {
    pub fn new(source: Arc<dyn BuildAdmissionHealthSource>) -> Self {
        Self { source }
    }

    fn wedged_finding(observation: &BuildAdmissionHealthObservation) -> Option<Finding> {
        let report = &observation.report;
        if report.readiness.is_healthy() {
            return None;
        }
        // Off never gates admission, so a non-healthy reason there denies
        // nothing and must not raise an alarm.
        if report.mode == BuildAdmissionMode::Off {
            return None;
        }
        if observation.unhealthy_for_seconds < DEFAULT_UNHEALTHY_WARN_SECONDS {
            return None;
        }
        let severity = severity_for_unhealthy(observation.unhealthy_for_seconds);
        let identities = json!(report.blocking_identities);
        let inputs = json!({
            "readiness": report.readiness.as_str(),
            "mode": format!("{:?}", report.mode),
            "unhealthy_for_seconds": observation.unhealthy_for_seconds,
            "create_unknown_pending": report.create_unknown_pending,
            "blocking_identities": identities,
            "blocking_identities_elided": report.blocking_identities_elided,
            "seconds_since_last_reconcile": report.seconds_since_last_reconcile,
        });
        let outputs = json!({
            "reason": REASON_ADMISSION_WEDGED,
            "severity": severity.as_str(),
            "readiness": report.readiness.as_str(),
            "admission_open": false,
        });
        // The detail names the rows. A finding that says
        // "CreateUnknownHealth for 14 minutes" leaves an operator exactly where
        // the outage left them; one that names the work_id does not.
        let named = if report.blocking_identities.is_empty() {
            "no journal row identities are recorded for this reason".to_owned()
        } else {
            let mut rendered = report.blocking_identities.join(", ");
            if report.blocking_identities_elided > 0 {
                rendered.push_str(&format!(" (+{} more)", report.blocking_identities_elided));
            }
            format!("blocking admission-journal rows: {rendered}")
        };
        let detail = format!(
            "build admission has been non-healthy for {}s with readiness `{}` (mode {:?}); \
             every Enforce admission is denied until it clears — {named}",
            observation.unhealthy_for_seconds,
            report.readiness.as_str(),
            report.mode,
        );
        let snapshot = ResolverSnapshot::new("resolve_build_admission_health", inputs, outputs);
        Some(
            Finding::new(
                severity,
                BUILD_ADMISSION_HEALTH_CHECK_NAME,
                snapshot,
                detail,
            )
            .with_entity_id("readiness", report.readiness.as_str())
            .with_evidence(json!({
                "reason": REASON_ADMISSION_WEDGED,
                "readiness": report.readiness.as_str(),
                "mode": format!("{:?}", report.mode),
                "draining": report.draining,
                "unhealthy_for_seconds": observation.unhealthy_for_seconds,
                "create_unknown_pending": report.create_unknown_pending,
                "blocking_identities": identities,
                "blocking_identities_elided": report.blocking_identities_elided,
                "seconds_since_last_reconcile": report.seconds_since_last_reconcile,
                "runbook": "server/docs/operational/stale-admission-occupancy.md",
            })),
        )
    }

    fn reconcile_stale_finding(observation: &BuildAdmissionHealthObservation) -> Option<Finding> {
        let report = &observation.report;
        if report.mode == BuildAdmissionMode::Off {
            return None;
        }
        // A pass that has never completed is only meaningful once enough time
        // has passed that one SHOULD have. Before that, a freshly-elected
        // leader would report its own startup as a dead reconciler.
        let age = match report.seconds_since_last_reconcile {
            Some(age) => age,
            None if observation.observed_for_seconds >= DEFAULT_RECONCILE_STALE_SECONDS => {
                observation.observed_for_seconds
            }
            None => return None,
        };
        if age < DEFAULT_RECONCILE_STALE_SECONDS {
            return None;
        }
        let ever = report.seconds_since_last_reconcile.is_some();
        let detail = if ever {
            format!(
                "the build-admission reconciliation loop has not completed a blocker-free \
                 pass for {age}s (threshold {DEFAULT_RECONCILE_STALE_SECONDS}s); occupying \
                 admission rows whose Kubernetes object is gone are no longer being reclaimed"
            )
        } else {
            format!(
                "the build-admission reconciliation loop has NEVER completed a blocker-free \
                 pass in {age}s of leadership; occupying admission rows are not being \
                 reclaimed at all"
            )
        };
        let inputs = json!({
            "seconds_since_last_reconcile": report.seconds_since_last_reconcile,
            "observed_for_seconds": observation.observed_for_seconds,
            "threshold_seconds": DEFAULT_RECONCILE_STALE_SECONDS,
            "readiness": report.readiness.as_str(),
        });
        let outputs = json!({
            "reason": REASON_RECONCILE_STALE,
            "severity": FindingSeverity::Error.as_str(),
            "ever_reconciled": ever,
        });
        let snapshot =
            ResolverSnapshot::new("resolve_build_admission_reconcile_age", inputs, outputs);
        Some(
            Finding::new(
                FindingSeverity::Error,
                BUILD_ADMISSION_HEALTH_CHECK_NAME,
                snapshot,
                detail,
            )
            .with_entity_id("reason", REASON_RECONCILE_STALE)
            .with_evidence(json!({
                "reason": REASON_RECONCILE_STALE,
                "seconds_since_last_reconcile": report.seconds_since_last_reconcile,
                "observed_for_seconds": observation.observed_for_seconds,
                "threshold_seconds": DEFAULT_RECONCILE_STALE_SECONDS,
                "ever_reconciled": ever,
                "readiness": report.readiness.as_str(),
                "runbook": "server/docs/operational/stale-admission-occupancy.md",
            })),
        )
    }
}

impl DoctorCheck for BuildAdmissionHealthCheck {
    fn name(&self) -> &'static str {
        BUILD_ADMISSION_HEALTH_CHECK_NAME
    }

    fn description(&self) -> &'static str {
        "Flags a build-admission controller that has been non-healthy beyond a bounded window, \
         or a reconciliation loop that has stopped completing passes, naming the admission-journal \
         rows responsible"
    }

    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        let Some(observation) = self.source.snapshot() else {
            return Ok(Vec::new());
        };
        let mut findings = Vec::new();
        if let Some(finding) = Self::wedged_finding(&observation) {
            findings.push(finding);
        }
        if let Some(finding) = Self::reconcile_stale_finding(&observation) {
            findings.push(finding);
        }
        Ok(findings)
    }
}

/// In-memory source for tests.
#[derive(Clone, Debug, Default)]
pub struct MemoryBuildAdmissionHealthSource {
    pub observation: Option<BuildAdmissionHealthObservation>,
}

impl MemoryBuildAdmissionHealthSource {
    #[must_use]
    pub fn new(observation: Option<BuildAdmissionHealthObservation>) -> Self {
        Self { observation }
    }
}

impl BuildAdmissionHealthSource for MemoryBuildAdmissionHealthSource {
    fn snapshot(&self) -> Option<BuildAdmissionHealthObservation> {
        self.observation.clone()
    }
}

/// Production source over the LEADER's live controller.
///
/// The elapsed-unhealthy clock is maintained here rather than on the
/// controller: the controller's readiness is derived from independent gates
/// with no notion of "since when", and a latch that only the observer needs
/// does not belong in the admission path.
///
/// Sampling happens on every `snapshot()` — that is, once per cheap doctor tick
/// — so the reported elapsed time is real observed wall time, never an
/// extrapolation.
pub struct ControllerBuildAdmissionHealthSource {
    controller: Option<Arc<BuildAdmissionController>>,
    started_at: Instant,
    /// The readiness label currently latched, and when it was first observed.
    /// `None` while healthy.
    unhealthy_since: Mutex<Option<(&'static str, Instant)>>,
}

impl ControllerBuildAdmissionHealthSource {
    #[must_use]
    pub fn new(controller: Option<Arc<BuildAdmissionController>>) -> Self {
        Self {
            controller,
            started_at: SystemClock::new().now_instant(),
            unhealthy_since: Mutex::new(None),
        }
    }

    /// Fold one readiness sample into the elapsed-unhealthy latch.
    ///
    /// A CHANGE of reason restarts the clock: "inventory pending for 10s then
    /// topology pending for 10s" is a startup walking its gates, not admission
    /// wedged for 20s.
    fn observe(&self, report: &BuildAdmissionHealthReport, now: Instant) -> i64 {
        let mut guard = self
            .unhealthy_since
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if report.readiness.is_healthy() {
            *guard = None;
            return 0;
        }
        let label = report.readiness.as_str();
        match guard.as_ref() {
            Some((latched, since)) if *latched == label => {
                i64::try_from(now.saturating_duration_since(*since).as_secs()).unwrap_or(i64::MAX)
            }
            _ => {
                *guard = Some((label, now));
                0
            }
        }
    }
}

impl BuildAdmissionHealthSource for ControllerBuildAdmissionHealthSource {
    fn snapshot(&self) -> Option<BuildAdmissionHealthObservation> {
        let controller = self.controller.as_ref()?;
        let report = controller.health_report();
        let now = SystemClock::new().now_instant();
        let unhealthy_for_seconds = self.observe(&report, now);
        Some(BuildAdmissionHealthObservation {
            report,
            unhealthy_for_seconds,
            observed_for_seconds: i64::try_from(
                now.saturating_duration_since(self.started_at).as_secs(),
            )
            .unwrap_or(i64::MAX),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_admission::BuildAdmissionReadiness;
    use std::time::Duration;

    fn report(readiness: BuildAdmissionReadiness) -> BuildAdmissionHealthReport {
        BuildAdmissionHealthReport {
            readiness,
            mode: BuildAdmissionMode::Enforce,
            draining: readiness == BuildAdmissionReadiness::ShutdownDraining,
            create_unknown_pending: u64::from(
                readiness == BuildAdmissionReadiness::CreateUnknownHealth,
            ),
            blocking_identities: if readiness == BuildAdmissionReadiness::CreateUnknownHealth {
                vec!["warm_build:proj-7:3@djinn-warm-proj-7-3".to_owned()]
            } else {
                Vec::new()
            },
            blocking_identities_elided: 0,
            seconds_since_last_reconcile: Some(30),
        }
    }

    fn observation(
        readiness: BuildAdmissionReadiness,
        unhealthy_for_seconds: i64,
    ) -> BuildAdmissionHealthObservation {
        BuildAdmissionHealthObservation {
            report: report(readiness),
            unhealthy_for_seconds,
            observed_for_seconds: unhealthy_for_seconds + 60,
        }
    }

    fn run(observation: Option<BuildAdmissionHealthObservation>) -> Vec<Finding> {
        BuildAdmissionHealthCheck::new(Arc::new(MemoryBuildAdmissionHealthSource::new(observation)))
            .run()
            .expect("run")
    }

    #[test]
    fn check_is_cheap_and_named() {
        let check =
            BuildAdmissionHealthCheck::new(Arc::new(MemoryBuildAdmissionHealthSource::default()));
        assert_eq!(check.name(), BUILD_ADMISSION_HEALTH_CHECK_NAME);
        assert_eq!(check.cadence(), DoctorCheckCadence::Cheap);
    }

    #[test]
    fn a_healthy_controller_produces_no_finding() {
        assert!(run(Some(observation(BuildAdmissionReadiness::Healthy, 0))).is_empty());
    }

    #[test]
    fn a_process_without_a_controller_produces_no_finding() {
        assert!(run(None).is_empty());
    }

    /// Startup walks several non-healthy gates. The window has to clear that,
    /// or the check would fire on every single boot and be turned off.
    #[test]
    fn a_briefly_unhealthy_controller_is_not_yet_a_finding() {
        assert!(
            run(Some(observation(
                BuildAdmissionReadiness::InventoryPending,
                30
            )))
            .is_empty()
        );
    }

    /// The core of the outage: the exact readiness reason and the row
    /// responsible must both reach the operator.
    #[test]
    fn a_wedged_controller_reports_the_reason_and_names_the_blocking_row() {
        let findings = run(Some(observation(
            BuildAdmissionReadiness::CreateUnknownHealth,
            600,
        )));
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.check_name, BUILD_ADMISSION_HEALTH_CHECK_NAME);
        assert_eq!(finding.severity, FindingSeverity::Warn);
        assert_eq!(finding.evidence["readiness"], "create_unknown_health");
        assert_eq!(finding.evidence["reason"], REASON_ADMISSION_WEDGED);
        assert_eq!(
            finding.evidence["blocking_identities"][0], "warm_build:proj-7:3@djinn-warm-proj-7-3",
            "a finding that reports only a COUNT is what cost five hours"
        );
        assert!(
            finding
                .detail
                .contains("warm_build:proj-7:3@djinn-warm-proj-7-3"),
            "the human-readable detail must name the row too: {}",
            finding.detail
        );
        assert!(
            finding.detail.contains("create_unknown_health"),
            "the detail must state the exact readiness reason: {}",
            finding.detail
        );
    }

    #[test]
    fn severity_escalates_with_the_wedge_duration() {
        assert_eq!(
            run(Some(observation(
                BuildAdmissionReadiness::TopologyPending,
                600
            )))[0]
                .severity,
            FindingSeverity::Warn
        );
        assert_eq!(
            run(Some(observation(
                BuildAdmissionReadiness::TopologyPending,
                1000
            )))[0]
                .severity,
            FindingSeverity::Error
        );
        assert_eq!(
            run(Some(observation(
                BuildAdmissionReadiness::TopologyPending,
                7200
            )))[0]
                .severity,
            FindingSeverity::Critical
        );
    }

    /// `ShutdownDraining` was an absolute latch with no clearing path anywhere.
    /// If it is ever set outside teardown it denies everything forever, so it
    /// must be reportable by name.
    #[test]
    fn a_stuck_draining_latch_is_reported_by_name() {
        let findings = run(Some(observation(
            BuildAdmissionReadiness::ShutdownDraining,
            1200,
        )));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence["readiness"], "shutdown_draining");
        assert_eq!(findings[0].evidence["draining"], true);
    }

    /// Off never denies, so a non-healthy reason there is not an alarm.
    #[test]
    fn off_mode_never_raises_an_alarm() {
        let mut sample = observation(BuildAdmissionReadiness::InventoryPending, 100_000);
        sample.report.mode = BuildAdmissionMode::Off;
        assert!(run(Some(sample)).is_empty());
    }

    /// The signal the audit called the single most valuable addition: nothing
    /// anywhere asserted that a reconcile pass had completed recently.
    #[test]
    fn a_stale_reconcile_age_is_a_finding_even_while_readiness_is_healthy() {
        let mut sample = observation(BuildAdmissionReadiness::Healthy, 0);
        sample.report.seconds_since_last_reconcile = Some(3600);
        let findings = run(Some(sample));
        assert_eq!(
            findings.len(),
            1,
            "a dead reconciler must be reported even before it has wedged anything"
        );
        assert_eq!(findings[0].evidence["reason"], REASON_RECONCILE_STALE);
        assert_eq!(findings[0].severity, FindingSeverity::Error);
        assert_eq!(findings[0].evidence["ever_reconciled"], true);
    }

    #[test]
    fn a_fresh_reconcile_age_is_not_a_finding() {
        let mut sample = observation(BuildAdmissionReadiness::Healthy, 0);
        sample.report.seconds_since_last_reconcile = Some(60);
        assert!(run(Some(sample)).is_empty());
    }

    /// A leader that has never completed a pass is the loudest version of the
    /// same condition — but only once enough time has passed that one should
    /// have run, so a freshly-elected leader does not accuse itself.
    #[test]
    fn never_having_reconciled_fires_only_after_a_pass_should_have_run() {
        let mut fresh = observation(BuildAdmissionReadiness::Healthy, 0);
        fresh.report.seconds_since_last_reconcile = None;
        fresh.observed_for_seconds = 60;
        assert!(run(Some(fresh)).is_empty());

        let mut aged = observation(BuildAdmissionReadiness::Healthy, 0);
        aged.report.seconds_since_last_reconcile = None;
        aged.observed_for_seconds = 3600;
        let findings = run(Some(aged));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].evidence["ever_reconciled"], false);
        assert!(findings[0].detail.contains("NEVER"));
    }

    /// The elapsed clock must restart when the REASON changes: a startup
    /// walking inventory → topology is not one wedge of the summed duration.
    #[test]
    fn a_changed_readiness_reason_restarts_the_elapsed_clock() {
        let source = ControllerBuildAdmissionHealthSource::new(None);
        let start = SystemClock::new().now_instant();
        let mut inventory = report(BuildAdmissionReadiness::InventoryPending);
        assert_eq!(source.observe(&inventory, start), 0);
        assert_eq!(
            source.observe(&inventory, start + Duration::from_secs(600)),
            600,
            "an unchanged reason accumulates"
        );
        inventory.readiness = BuildAdmissionReadiness::TopologyPending;
        assert_eq!(
            source.observe(&inventory, start + Duration::from_secs(601)),
            0,
            "a different reason is a different episode"
        );
    }

    #[test]
    fn becoming_healthy_clears_the_elapsed_clock() {
        let source = ControllerBuildAdmissionHealthSource::new(None);
        let start = SystemClock::new().now_instant();
        let pending = report(BuildAdmissionReadiness::InventoryPending);
        source.observe(&pending, start);
        assert_eq!(
            source.observe(
                &report(BuildAdmissionReadiness::Healthy),
                start + Duration::from_secs(60)
            ),
            0
        );
        assert_eq!(
            source.observe(&pending, start + Duration::from_secs(61)),
            0,
            "a healed-then-degraded controller starts a NEW episode"
        );
    }

    /// A source with no controller must yield no observation — and therefore no
    /// finding — rather than fabricating a healthy one.
    #[test]
    fn a_controllerless_source_yields_no_observation() {
        assert!(
            ControllerBuildAdmissionHealthSource::new(None)
                .snapshot()
                .is_none()
        );
    }
}
