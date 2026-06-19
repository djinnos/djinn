use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{Request, StatusCode};
use djinn_db::{
    AgentRepository, CreateUserAuthSession, ProjectRepository, SessionAuthRepository,
    UserRepository,
};
use http_body_util::BodyExt;
use tower::ServiceExt;

use crate::events::EventBus;
use crate::test_helpers;

async fn seed_admin_session(db: &djinn_db::Database) -> String {
    let user = UserRepository::new(db.clone())
        .upsert_from_github(rand_github_id(), "agents-admin", None, None)
        .await
        .unwrap();
    UserRepository::new(db.clone())
        .set_admin_status(&user.id, true)
        .await
        .unwrap();

    let token = format!("agents-sess-{}", uuid::Uuid::now_v7().simple());
    SessionAuthRepository::new(db.clone())
        .create(CreateUserAuthSession {
            token: &token,
            user_fk: &user.id,
            github_login: "agents-admin",
            github_name: None,
            github_avatar_url: None,
            github_access_token: "gho_agents_test",
            github_access_token_expires_at: None,
            github_refresh_token: None,
            github_refresh_token_expires_at: None,
            expires_at: "2099-01-01T00:00:00.000Z",
        })
        .await
        .unwrap();
    token
}

fn rand_github_id() -> i64 {
    let bytes = *uuid::Uuid::now_v7().as_bytes();
    i64::from_be_bytes(bytes[8..16].try_into().unwrap()).unsigned_abs() as i64
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn put_update_rejects_direct_learned_prompt_set() {
    let db = test_helpers::create_test_db();
    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create("agents-rest-lp-reject", "djinn", "agents-rest-lp-reject")
        .await
        .unwrap();
    let admin_cookie = seed_admin_session(&db).await;

    let agent_repo = AgentRepository::new(db.clone(), EventBus::noop());
    let default_agent = agent_repo
        .get_default_for_base_role(&project.id, "architect")
        .await
        .unwrap()
        .expect("project creation should seed an architect default agent");
    agent_repo
        .append_learned_prompt(
            &default_agent.id,
            "Prefer bounded-context design notes.",
            Some(r#"{"success_rate":0.9}"#),
        )
        .await
        .unwrap();

    let app = test_helpers::create_test_app_with_db(db.clone());
    let payload = serde_json::json!({
        "system_prompt_extensions": ["Human-authored architect instructions"],
        "learned_prompt": "Direct override attempt",
    });
    let request = Request::builder()
        .method("PUT")
        .uri(format!("/api/agents/{}", default_agent.id))
        .header(CONTENT_TYPE, "application/json")
        .header("cookie", format!("djinn_session={admin_cookie}"))
        .body(Body::from(payload.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8(body.to_vec()).unwrap();
    assert!(body.contains("Direct learned_prompt setting is deprecated"));

    let persisted = AgentRepository::new(db, EventBus::noop())
        .get(&default_agent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        persisted.learned_prompt.as_deref(),
        Some("Prefer bounded-context design notes.")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_clear_learned_prompt_clears_derived_state() {
    let db = test_helpers::create_test_db();
    let project = ProjectRepository::new(db.clone(), EventBus::noop())
        .create("agents-rest-lp-clear", "djinn", "agents-rest-lp-clear")
        .await
        .unwrap();
    let admin_cookie = seed_admin_session(&db).await;

    let agent_repo = AgentRepository::new(db.clone(), EventBus::noop());
    let default_agent = agent_repo
        .get_default_for_base_role(&project.id, "architect")
        .await
        .unwrap()
        .expect("project creation should seed an architect default agent");
    agent_repo
        .append_learned_prompt(
            &default_agent.id,
            "Prefer bounded-context design notes.",
            Some(r#"{"success_rate":0.9}"#),
        )
        .await
        .unwrap();

    let app = test_helpers::create_test_app_with_db(db.clone());
    let request = Request::builder()
        .method("DELETE")
        .uri(format!("/api/agents/{}/learned-prompt", default_agent.id))
        .header("cookie", format!("djinn_session={admin_cookie}"))
        .body(Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let persisted = AgentRepository::new(db, EventBus::noop())
        .get(&default_agent.id)
        .await
        .unwrap()
        .unwrap();
    assert!(persisted.learned_prompt.is_none());
}
