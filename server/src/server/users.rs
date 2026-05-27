// HTTP handler for the /users REST endpoint consumed by the desktop frontend.
//
// Surfaces the org's user roster so the UI can resolve a task's
// `created_by_user_id` to a human-readable name/avatar (e.g. the Kanban
// owner filter). The list is small (one row per human who has ever signed
// in) so it is returned unpaginated, mirroring `UserRepository::list_all`.

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use serde::Serialize;

use crate::server::AppState;
use djinn_db::{User, UserRepository};

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/users", get(list_users))
}

#[derive(Serialize)]
struct UserResponse {
    id: String,
    github_login: String,
    github_name: Option<String>,
    github_avatar_url: Option<String>,
    is_member_of_org: bool,
    is_admin: bool,
    last_seen_at: Option<String>,
}

impl From<&User> for UserResponse {
    fn from(u: &User) -> Self {
        Self {
            id: u.id.clone(),
            github_login: u.github_login.clone(),
            github_name: u.github_name.clone(),
            github_avatar_url: u.github_avatar_url.clone(),
            is_member_of_org: u.is_member_of_org,
            is_admin: u.is_admin,
            last_seen_at: u.last_seen_at.clone(),
        }
    }
}

#[derive(Serialize)]
struct ListResponse {
    users: Vec<UserResponse>,
}

async fn list_users(
    State(state): State<AppState>,
) -> Result<Json<ListResponse>, (StatusCode, String)> {
    let repo = UserRepository::new(state.db().clone());
    let users = repo
        .list_all()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(ListResponse {
        users: users.iter().map(UserResponse::from).collect(),
    }))
}
