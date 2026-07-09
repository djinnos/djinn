use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use djinn_agent::actors::coordinator::{DebugDispatchState, DebugTotals};

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

    let totals = DebugTotals {
        cooldowns_active: coordinator_snapshot.cooldowns.len(),
        inflight_ledger_size: coordinator_snapshot.inflight_ledger.len(),
        free_slots: slot_pool.iter().filter(|slot| slot.state == "free").count(),
        busy_slots: slot_pool.iter().filter(|slot| slot.state == "busy").count(),
        open_breakers: breaker.iter().filter(|entry| entry.state == "open").count(),
    };

    let response = DebugDispatchState {
        snapshot_at: snapshot_at_now(),
        cooldowns: coordinator_snapshot.cooldowns,
        failure_streaks: coordinator_snapshot.failure_streaks,
        inflight_ledger: coordinator_snapshot.inflight_ledger,
        slot_pool,
        breaker,
        paused,
        totals,
    };

    let body = serde_json::to_string_pretty(&response).map_err(internal_error)?;
    Ok((
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response())
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
}
