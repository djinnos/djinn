use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};

use crate::server::AppState;

/// Axum middleware that validates the `Authorization: Bearer <token>` header against Clerk JWKS.
/// Injects [`super::Claims`] as a request extension on success.
/// No-ops (passes through) when the AppState has no JWKS cache configured.
pub async fn require_auth(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(jwks) = state.jwks() else {
        return Ok(next.run(req).await);
    };

    let token = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let claims = jwks.validate(token).await.map_err(|e| {
        tracing::warn!(error = %e, "JWT validation failed");
        match e {
            super::AuthError::TokenExpired => StatusCode::UNAUTHORIZED,
            _ => StatusCode::UNAUTHORIZED,
        }
    })?;

    tracing::debug!(user_id = %claims.sub, "authenticated MCP session");
    req.extensions_mut().insert(claims);
    Ok(next.run(req).await)
}
