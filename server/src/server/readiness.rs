//! Browser-facing authenticated readiness routes.
//!
//! This module is deliberately only a transport boundary: authorization and all
//! readiness persistence stay behind the control-plane services.

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use djinn_control_plane::{
    readiness_kickoff::{ReadinessKickoffError, ReadinessKickoffRequest, ReadinessKickoffService},
    readiness_query::{
        ReadinessProjectQuery, ReadinessQueryError, ReadinessQueryService, ReadinessRunQuery,
        ReadinessRunSummary,
    },
};
use serde::{Deserialize, Serialize};

use crate::{readiness_pin_resolver::AgentNativeReadinessPinResolver, server::AppState};

const READINESS_ROUTE: &str = "/api/projects/{project_id}/readiness";
const READINESS_DETAIL_ROUTE: &str = "/api/projects/{project_id}/readiness/{run_id}";
const READINESS_KICKOFF_ROUTE: &str = "/api/projects/{project_id}/readiness/kickoff";

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route(READINESS_ROUTE, get(active_or_latest))
        .route(READINESS_DETAIL_ROUTE, get(run_detail))
        .route(READINESS_KICKOFF_ROUTE, post(kickoff))
}

#[derive(Debug, Deserialize)]
struct KickoffRequest {
    idempotency_key: String,
}

/// Materialization metadata lets a browser distinguish its newly-started run
/// from the project's already-active run that absorbed its repeated request.
#[derive(Debug, Serialize)]
struct KickoffResponse {
    run: ReadinessRunSummary,
    identification_task_id: String,
    created: bool,
    reused: bool,
}

async fn active_or_latest(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let user_id = match authenticated_user_id(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    match ReadinessQueryService::new(state.db().clone())
        .active_or_latest(ReadinessProjectQuery {
            project_id,
            authenticated_owner_id: user_id,
        })
        .await
    {
        Ok(summary) => Json(summary).into_response(),
        Err(error) => query_error_response(error),
    }
}

async fn run_detail(
    State(state): State<AppState>,
    Path((project_id, run_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let user_id = match authenticated_user_id(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    match ReadinessQueryService::new(state.db().clone())
        .run_detail(ReadinessRunQuery {
            project_id,
            run_id,
            authenticated_owner_id: user_id,
        })
        .await
    {
        Ok(detail) => Json(detail).into_response(),
        Err(error) => query_error_response(error),
    }
}

async fn kickoff(
    State(state): State<AppState>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<KickoffRequest>,
) -> Response {
    let user_id = match authenticated_user_id(&state, &headers).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    if request.idempotency_key.trim().is_empty() {
        return kickoff_error_response(ReadinessKickoffError::BlankIdempotencyKey);
    }

    // Ask the query service, rather than a repository, whether the project had
    // an active run before materialization. The materializer serializes by
    // project and returns that active run on a repeated kickoff.
    let query_service = ReadinessQueryService::new(state.db().clone());
    let prior_active = match query_service
        .active_or_latest(ReadinessProjectQuery {
            project_id: project_id.clone(),
            authenticated_owner_id: user_id.clone(),
        })
        .await
    {
        Ok(summary) => summary.filter(|run| is_active(&run.status)),
        Err(error) => return query_error_response(error),
    };

    match ReadinessKickoffService::new(state.db().clone(), AgentNativeReadinessPinResolver)
        .kickoff(ReadinessKickoffRequest {
            project_id,
            authenticated_owner_id: user_id,
            idempotency_key: request.idempotency_key,
        })
        .await
    {
        Ok(materialization) => {
            let run: ReadinessRunSummary = materialization.run.into();
            let reused = prior_active
                .as_ref()
                .is_some_and(|prior| prior.id == run.id);
            Json(KickoffResponse {
                run,
                identification_task_id: materialization.identification_task.id,
                created: !reused,
                reused,
            })
            .into_response()
        }
        Err(error) => kickoff_error_response(error),
    }
}

fn is_active(status: &str) -> bool {
    matches!(status, "identifying" | "analyzing" | "aggregating")
}

async fn authenticated_user_id(state: &AppState, headers: &HeaderMap) -> Result<String, Response> {
    match super::oauth::resolve_request_user(state, headers).await {
        super::oauth::AuthOutcome::User(user) => Ok(user.user_id),
        super::oauth::AuthOutcome::Missing | super::oauth::AuthOutcome::InvalidBearer => Err(
            error_response(StatusCode::UNAUTHORIZED, "authentication_required"),
        ),
    }
}

fn query_error_response(error: ReadinessQueryError) -> Response {
    let (status, code) = match error {
        ReadinessQueryError::ProjectNotFound { .. }
        | ReadinessQueryError::RunNotFound { .. }
        | ReadinessQueryError::RunProjectMismatch { .. } => (StatusCode::NOT_FOUND, "not_found"),
        ReadinessQueryError::AuthenticatedOwnerNotFound { .. } => {
            (StatusCode::UNAUTHORIZED, "authentication_required")
        }
        ReadinessQueryError::Persistence { .. } => {
            (StatusCode::INTERNAL_SERVER_ERROR, "persistence_failure")
        }
    };
    error_response(status, code)
}

fn kickoff_error_response(error: ReadinessKickoffError) -> Response {
    let (status, code) = match error {
        ReadinessKickoffError::BlankIdempotencyKey => {
            (StatusCode::BAD_REQUEST, "blank_idempotency_key")
        }
        ReadinessKickoffError::ProjectNotFound { .. } => (StatusCode::NOT_FOUND, "not_found"),
        ReadinessKickoffError::AuthenticatedOwnerNotFound { .. } => {
            (StatusCode::UNAUTHORIZED, "authentication_required")
        }
        ReadinessKickoffError::ImmutableSnapshotUnavailable { .. }
        | ReadinessKickoffError::SkillPinUnavailable { .. }
        | ReadinessKickoffError::SkillPinMismatch { .. } => {
            (StatusCode::CONFLICT, "kickoff_unavailable")
        }
        ReadinessKickoffError::Persistence { .. } => {
            (StatusCode::INTERNAL_SERVER_ERROR, "persistence_failure")
        }
    };
    error_response(status, code)
}

fn error_response(status: StatusCode, code: &'static str) -> Response {
    (status, Json(serde_json::json!({ "code": code }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_statuses_are_limited_to_materializer_active_states() {
        assert!(is_active("identifying"));
        assert!(is_active("analyzing"));
        assert!(is_active("aggregating"));
        assert!(!is_active("completed"));
        assert!(!is_active("failed"));
    }

    #[test]
    fn service_errors_have_stable_transport_statuses() {
        assert_eq!(
            kickoff_error_response(ReadinessKickoffError::BlankIdempotencyKey).status(),
            StatusCode::BAD_REQUEST
        );
        // A missing user row is an authentication failure, never a silent
        // success: dropping the owner-equality gate must not admit a caller
        // whose identity cannot be resolved.
        assert_eq!(
            query_error_response(ReadinessQueryError::AuthenticatedOwnerNotFound {
                user_id: "user".into(),
            })
            .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            kickoff_error_response(ReadinessKickoffError::AuthenticatedOwnerNotFound {
                user_id: "user".into(),
            })
            .status(),
            StatusCode::UNAUTHORIZED
        );
        // Project scoping is unchanged: a run reached through the wrong
        // project is still not found.
        assert_eq!(
            query_error_response(ReadinessQueryError::RunProjectMismatch {
                project_id: "project".into(),
                run_id: "run".into(),
                actual_project_id: "other".into(),
            })
            .status(),
            StatusCode::NOT_FOUND
        );
    }
}
