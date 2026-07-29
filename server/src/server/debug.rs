//! `/debug/dispatch-state`: the admin answer to "why is nothing dispatching".
//!
//! # The 2026-07-29 gap
//!
//! For five hours the build-admission controller denied every dispatch on the
//! board with `cause: "controller_not_admitting"`, and this endpoint did not
//! mention build admission at all. Everything it *did* report was healthy —
//! no cooldowns, an idle slot pool, no open breaker, dispatch not paused — so
//! reading it actively pointed away from the fault. Diagnosis required ssh to
//! the node and `grep readiness=` over container logs.
//!
//! `AppState.inner.build_admission` was same-crate the whole time. It is now
//! reported, including the full set of unsatisfied readiness gates.
//!
//! # A note on `/health`
//!
//! `/health` returns `{"status":"ok"}` unconditionally and still does. That is
//! deliberate and is left alone here: it is a Kubernetes liveness/readiness
//! probe, and a process that is up and serving is genuinely live. Making it
//! fail on a closed admission gate would make the kubelet restart the pod, and
//! a restart is precisely what re-armed the settle window on 2026-07-29 and
//! extended the outage by ten more minutes. A wedged admission gate is a
//! *board* health problem, not a *process* health problem, and the two must
//! not share a status code. `/debug/dispatch-state` (admin-authenticated) and
//! `board_health` are the right surfaces, and both now carry it.

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use djinn_agent::actors::coordinator::{
    BuildAdmissionMode, DebugBuildAdmission, DebugDispatchState, DebugTotals,
};

use crate::server::{AppState, auth};

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/debug/dispatch-state", get(debug_dispatch_state))
}

async fn debug_dispatch_state(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    auth::require_admin(&state, &headers).await?;

    let coordinator = state.coordinator().await.ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "coordinator is not initialized".to_string(),
        )
    })?;
    let slot_pool = state.pool().await.ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "slot pool is not initialized".to_string(),
        )
    })?;

    let coordinator_snapshot = coordinator
        .debug_dispatch_state()
        .await
        .map_err(internal_error)?;
    let slot_pool = slot_pool.snapshot().await.map_err(internal_error)?;
    let breaker = state.health_tracker().debug_snapshot();
    let pause_state = djinn_agent::dispatch_pause::load_dispatch_pause_state(
        state.db().clone(),
        state.event_bus(),
    )
    .await
    .map_err(internal_error)?;
    let paused = djinn_agent::dispatch_pause::debug_view(&pause_state);

    // `None` only when admission is Off and no controller exists. Every other
    // shape — including a controller that is denying everything — reports.
    let build_admission = build_admission_view(&state).await;

    let totals = DebugTotals {
        cooldowns_active: coordinator_snapshot.cooldowns.len(),
        inflight_ledger_size: coordinator_snapshot.inflight_ledger.len(),
        free_slots: slot_pool.iter().filter(|slot| slot.state == "free").count(),
        busy_slots: slot_pool.iter().filter(|slot| slot.state == "busy").count(),
        open_breakers: breaker.iter().filter(|entry| entry.state == "open").count(),
        build_admission_denying_all: denying_all(build_admission.as_ref()),
    };

    let response = DebugDispatchState {
        snapshot_at: snapshot_at_now(),
        cooldowns: coordinator_snapshot.cooldowns,
        failure_streaks: coordinator_snapshot.failure_streaks,
        inflight_ledger: coordinator_snapshot.inflight_ledger,
        slot_pool,
        breaker,
        paused,
        build_admission,
        totals,
    };

    let body = serde_json::to_string_pretty(&response).map_err(internal_error)?;
    Ok((
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response())
}

/// Is the build-admission controller currently refusing every dispatch?
///
/// The single field that would have ended the 2026-07-29 investigation in
/// seconds. `mode == "enforce"` is load-bearing rather than decorative: under
/// `off` or `observe` a failing readiness gate denies nothing at all, and
/// asserting a board-wide denial from readiness alone would be exactly the
/// fabricated-number failure #2661 removed from the denial log.
pub(super) fn denying_all(admission: Option<&DebugBuildAdmission>) -> bool {
    admission.is_some_and(|admission| admission.mode == "enforce" && !admission.is_ready)
}

/// Project the build-admission controller into its wire shape.
///
/// Returns `None` only when no controller was constructed (admission `Off`),
/// which the payload renders as an explicit `null` rather than a missing key.
async fn build_admission_view(state: &AppState) -> Option<DebugBuildAdmission> {
    let controller = state.build_admission()?;
    let snapshot = controller.debug_snapshot().await;
    let health = snapshot.health;
    Some(DebugBuildAdmission {
        readiness: health.readiness.as_str().to_owned(),
        is_ready: health.readiness.is_healthy(),
        unsatisfied_gates: snapshot
            .unsatisfied_gates
            .iter()
            .map(|gate| (*gate).to_owned())
            .collect(),
        mode: match health.mode {
            BuildAdmissionMode::Off => "off",
            BuildAdmissionMode::Observe => "observe",
            BuildAdmissionMode::Enforce => "enforce",
        }
        .to_owned(),
        effective_cap: snapshot.effective_cap,
        configured_cap: snapshot.configured_cap,
        occupancy: snapshot.occupancy,
        create_unknown_pending: health.create_unknown_pending,
        blocking_identities: health.blocking_identities,
        blocking_identities_elided: health.blocking_identities_elided,
        seconds_since_last_reconcile: health.seconds_since_last_reconcile,
        server_epoch: snapshot.server_epoch,
        queued: snapshot.queued,
    })
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn snapshot_at_now() -> String {
    let now = time::OffsetDateTime::now_utc();
    let format = time::format_description::parse_borrowed::<3>(
        "[year]-[month]-[day]T[hour]:[minute]:[second]",
    )
    .expect("valid timestamp format");
    let prefix = now.format(&format).expect("UTC timestamp should format");
    format!("{prefix}.{:03}Z", now.millisecond())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_at_now_uses_millisecond_rfc3339_utc_shape() {
        let value = snapshot_at_now();
        assert_eq!(value.len(), "2026-06-15T17:30:00.123Z".len());
        assert!(value.ends_with('Z'));
        assert_eq!(value.as_bytes()[19], b'.');
    }

    fn admission(mode: &str, readiness: &str) -> DebugBuildAdmission {
        DebugBuildAdmission {
            readiness: readiness.to_owned(),
            is_ready: readiness == "healthy",
            unsatisfied_gates: if readiness == "healthy" {
                Vec::new()
            } else {
                vec![readiness.to_owned()]
            },
            mode: mode.to_owned(),
            effective_cap: 3,
            configured_cap: 3,
            occupancy: None,
            create_unknown_pending: u64::from(readiness == "create_unknown_health"),
            blocking_identities: Vec::new(),
            blocking_identities_elided: 0,
            seconds_since_last_reconcile: None,
            server_epoch: "epoch-1".to_owned(),
            queued: 0,
        }
    }

    /// **The 2026-07-29 shape.** Enforcing, latched on `CreateUnknownHealth`,
    /// denying every dispatch on the board. The endpoint must say so in
    /// `totals`, which is the block an operator scans first.
    #[test]
    fn an_enforcing_unready_controller_is_flagged_as_denying_everything() {
        let view = admission("enforce", "create_unknown_health");
        assert!(denying_all(Some(&view)));
    }

    /// **Neutralisation guard.** Readiness alone is not a denial. Under
    /// `observe` the controller records the same degradation and admits
    /// everything, so claiming a board-wide denial would be a fabrication.
    #[test]
    fn a_non_enforcing_controller_is_never_flagged_as_denying() {
        for mode in ["off", "observe"] {
            let view = admission(mode, "create_unknown_health");
            assert!(
                !denying_all(Some(&view)),
                "mode `{mode}` denies nothing, whatever its readiness"
            );
        }
    }

    /// A healthy enforcing controller, and an absent one, are both quiet.
    #[test]
    fn a_healthy_or_absent_controller_is_quiet() {
        assert!(!denying_all(Some(&admission("enforce", "healthy"))));
        assert!(!denying_all(None));
    }
}
