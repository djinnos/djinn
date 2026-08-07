//! The coordinator half of CI-routing reporting and the rollback quiescence
//! report (proposal `nafu`, wave 5).
//!
//! The repository can answer four of the rollback gate's six counts. It cannot
//! answer the other two, and no amount of SQL will change that:
//!
//! * **registered provider-action futures** live in this process's
//!   [`ProviderActionScope`]. A `calling` row proves a call *episode* was
//!   claimed; the scope is what knows whether a future is still running behind
//!   it. They are not the same number and the difference is exactly the window
//!   in which a rollback would strand a live `rerun_failed_jobs`.
//! * the **evidence-advance high-watermark** is a query, but it is the one
//!   query whose answer decides whether an old binary — which ignores route
//!   rows entirely — would handle the same failure a second time.
//!
//! So the report is assembled here, where both halves are visible, and stored
//! as one row. See [`CiRouteAttemptRepository::record_rollback_quiescence_report`].
//!
//! [`ProviderActionScope`]: crate::types::ProviderActionScope
//! [`CiRouteAttemptRepository::record_rollback_quiescence_report`]: djinn_db::CiRouteAttemptRepository::record_rollback_quiescence_report

use djinn_db::{
    CiRollbackQuiescenceReport, CiRouteQuiescenceAttestation, CiRouteReport, CiRouteReportFilter,
};

use crate::actor::CoordinatorActor;

impl CoordinatorActor {
    /// The routing report for one lane/time window.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn ci_route_report(
        &self,
        filter: &CiRouteReportFilter,
    ) -> Result<CiRouteReport, String> {
        self.ci_routes()
            .route_report(filter)
            .await
            .map_err(|error| error.to_string())
    }

    /// Log one unscoped routing report, if there is anything to report.
    ///
    /// Called from the periodic route sweep. Silent when the feature has never
    /// created a route, so a deployment with the gate off pays one aggregate
    /// query per sweep and prints nothing.
    pub(crate) async fn emit_ci_route_report(&self) {
        let report = match self.ci_route_report(&CiRouteReportFilter::all()).await {
            Ok(report) => report,
            Err(error) => {
                tracing::warn!(%error, "ci route: routing report failed");
                return;
            }
        };
        if report.tier2_leases_opened == 0
            && report.charged_slots == 0
            && report.held == 0
            && report.total_obsolete_suppressions() == 0
        {
            return;
        }
        tracing::info!(
            // classification and action
            class_inconclusive = report.class_inconclusive,
            class_causal_failure = report.class_causal_failure,
            class_unknown = report.class_unknown,
            action_rerun_run = report.action_rerun_run,
            action_reenqueue = report.action_reenqueue,
            action_ask_lead = report.action_ask_lead,
            // provider
            retriggered = report.retriggered,
            reenqueued = report.reenqueued,
            provider_action_failures = report.provider_action_failures,
            outcome_unknown = report.outcome_unknown,
            // the three suppression boundaries, reported separately
            suppressed_before_provider_call = report.suppressed_before_provider_call,
            suppressed_before_lead_dispatch = report.suppressed_before_lead_dispatch,
            suppressed_before_supervisor_apply = report.suppressed_before_supervisor_apply,
            held = report.held,
            // recovery and budget
            resumed_pre_call_recoveries = report.resumed_pre_call_recoveries,
            quiescent_owner_recoveries = report.quiescent_owner_recoveries,
            live_owner_recovery_deferrals = report.live_owner_recovery_deferrals,
            charged_slots = report.charged_slots,
            retry_exhausted = report.retry_exhausted,
            // Tier 2 — note the two diagnostic splits, which is the whole
            // reason `lead_rejection` is a durable column.
            lead_invocations = report.lead_invocations,
            repair_reopens = report.repair_reopens,
            diagnostic_reopens = report.diagnostic_reopens,
            diagnostic_reopens_from_rejected_result =
                report.diagnostic_reopens_from_rejected_result,
            diagnostic_reopens_from_absent_result = report.diagnostic_reopens_from_absent_result,
            parks_with_cited_cause = report.parks_with_cited_cause,
            // worker cost
            worker_reopens = report.worker_reopens,
            merged_prs = report.merged_prs,
            lead_sessions_per_merged_pr = ?report.lead_sessions_per_merged_pr(),
            worker_reopens_per_merged_pr = ?report.worker_reopens_per_merged_pr(),
            "ci route report"
        );
    }

    /// Take and durably record one rollback quiescence report.
    ///
    /// This is the gate the proposal puts in front of a binary rollback. It is
    /// recorded whether or not it permits one: a blocked attempt is precisely
    /// what an operator needs to be able to point at afterwards, and a report
    /// that only exists when it says "yes" is a report nobody can audit.
    ///
    /// # Errors
    ///
    /// Database failures.
    pub async fn record_ci_rollback_quiescence_report(
        &self,
    ) -> Result<CiRollbackQuiescenceReport, String> {
        let routes = self.ci_routes();

        // The in-process half. `in_flight()` counts guards that have been
        // admitted and not yet dropped — i.e. futures that may still be talking
        // to GitHub. Reading it *before* the repository counts is deliberate:
        // an over-count blocks a rollback (safe), an under-count permits one
        // (not), and a future admitted after this read will be visible to the
        // next report rather than missed by this one.
        let registered_provider_futures =
            i64::try_from(self.provider_action_scope.in_flight()).unwrap_or(i64::MAX);
        let current_failed_identities = routes
            .current_failed_identity_count()
            .await
            .map_err(|error| error.to_string())?;

        let report = routes
            .record_rollback_quiescence_report(
                self.ci_routing_gate().as_str(),
                &self.coordinator_incarnation_id,
                CiRouteQuiescenceAttestation {
                    registered_provider_futures,
                    current_failed_identities,
                },
            )
            .await
            .map_err(|error| error.to_string())?;

        if report.permits_rollback {
            tracing::info!(
                report_id = %report.id,
                gate = %report.gate_state,
                "ci route: rollback quiescence report is clean — binary rollback permitted"
            );
        } else {
            tracing::warn!(
                report_id = %report.id,
                gate = %report.gate_state,
                blocking = ?report.blocking_reasons(),
                "ci route: rollback quiescence report is not clean — binary rollback blocked"
            );
        }
        Ok(report)
    }
}
