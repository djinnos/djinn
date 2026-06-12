use axum::{routing::get, Router};

pub fn router() -> Router<()> {
    Router::new().route("/api/fixture", get(list_agents))
}

pub async fn list_agents() {}
