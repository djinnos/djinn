//! End-to-end replica of the coordinator's `resolve_user_model_priority` body
//! (real DB + repos + catalog).
//!
//! This test used to live inside `CredentialRepository`'s own `mod tests`. When
//! that repository moved to `djinn-db` it could not come along: it needs
//! `djinn_provider::catalog::CatalogService`, and `djinn-db` must not depend on
//! `djinn-provider`. It lives here instead — on the provider side of the single
//! `djinn-provider -> djinn-db` edge, where both halves are in scope.

use djinn_core::events::EventBus;
use djinn_core::models::ModelLanes;
use djinn_db::{CredentialRepository, Database, UserRepository, UserSettingsRepository};
use djinn_provider::catalog::CatalogService;

/// Seed a real `users` row so the `credentials.owner_user_id` FK holds.
async fn seed_user(db: &Database, github_id: i64, login: &str) -> String {
    db.ensure_initialized().await.unwrap();
    UserRepository::new(db.clone())
        .upsert_from_github(github_id, login, None, None)
        .await
        .unwrap()
        .id
}

/// Reproduces the exact staging setup: a user who selected `openai/gpt-5.5` and
/// connected Codex (`chatgpt_codex` OAuth, which merges into `openai`) plus an
/// org-shared fireworks key. The per-method `#[cfg(test)]` stub means the live
/// suite never exercises this, so we replicate it inline. The model MUST stay
/// eligible.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_model_priority_keeps_openai_via_codex_merge() {
    let db = Database::open_in_memory().unwrap();
    let uid = seed_user(&db, 9001, "fern").await;
    let repo = CredentialRepository::new(db.clone(), EventBus::noop());
    repo.set_with_owner("chatgpt_codex", "__OAUTH_CHATGPT_CODEX", "tok", Some(&uid))
        .await
        .unwrap();
    repo.set_with_owner("fireworks-ai", "FIREWORKS_API_KEY", "fk", None)
        .await
        .unwrap();
    UserSettingsRepository::new(db.clone())
        .upsert_lanes(
            &uid,
            &ModelLanes::from_flat(vec!["openai/gpt-5.5".to_string()]),
        )
        .await
        .unwrap();

    // ── replicate resolve_user_model_priority (implement lane = worker) ──
    let models = UserSettingsRepository::new(db.clone())
        .get(&uid)
        .await
        .unwrap()
        .unwrap()
        .lanes
        .unwrap()
        .for_role("worker")
        .to_vec();
    let creds = repo.list_for_user(Some(&uid)).await.unwrap();
    let connected = CatalogService::new().connected_provider_ids(&creds);
    let result: Vec<String> = models
        .into_iter()
        .filter(|m| {
            let p = m.split_once('/').map(|(p, _)| p).unwrap_or(m.as_str());
            connected.contains(p)
        })
        .collect();

    assert!(
        connected.contains("openai"),
        "codex merge should connect openai; connected={connected:?}"
    );
    assert_eq!(
        result,
        vec!["openai/gpt-5.5".to_string()],
        "openai/gpt-5.5 must remain eligible; connected={connected:?}"
    );
}
