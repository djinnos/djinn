use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use djinn_agent::{DebugDispatchState, DebugTotals, DispatchPauseView};
use djinn_db::DispatchPauseRepository;

use crate::server::auth::require_admin;
use crate::server::state::AppState;

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/debug/dispatch-state", get(debug_dispatch_state))
}

async fn debug_dispatch_state(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, (axum::http::StatusCode, String)> {
    require_admin(&state, &headers).await?;

    let coordinator = match state.coordinator().await {
        Some(handle) => handle
            .debug_dispatch_state()
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
        None => djinn_agent::CoordinatorDebugSnapshot {
            cooldowns: Vec::new(),
            failure_streaks: Vec::new(),
            inflight_ledger: Vec::new(),
        },
    };

    let slot_pool = match state.pool().await {
        Some(pool) => pool
            .snapshot()
            .await
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?,
        None => Vec::new(),
    };

    let breaker = state.health_tracker().debug_snapshot();
    let pause_state = DispatchPauseRepository::new(state.db().clone(), state.event_bus())
        .get_status()
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let paused: DispatchPauseView = djinn_agent::dispatch_pause::debug_view(&pause_state);

    let totals = DebugTotals {
        cooldowns_active: coordinator.cooldowns.len(),
        inflight_ledger_size: coordinator.inflight_ledger.len(),
        free_slots: slot_pool.iter().filter(|slot| slot.state == "free").count(),
        busy_slots: slot_pool.iter().filter(|slot| slot.state == "busy").count(),
        open_breakers: breaker.iter().filter(|entry| entry.state == "open").count(),
    };

    let state = DebugDispatchState {
        snapshot_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc().to_string()),
        cooldowns: coordinator.cooldowns,
        failure_streaks: coordinator.failure_streaks,
        inflight_ledger: coordinator.inflight_ledger,
        slot_pool,
        breaker,
        paused,
        totals,
    };

    let body = serde_json::to_string_pretty(&state)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(([(CONTENT_TYPE, "application/json; charset=utf-8")], body).into_response())
}
