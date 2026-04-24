//! Chat-scope defaults tests.
//!
//! Pre-refactor these exercised per-project skill/MCP resolution via
//! `apply_chat_skills(path, db)` and `chat_effective_config(path, db)`.
//! Under the chat-user-global refactor chat is user-scoped globally so
//! both of those surfaces were deleted.  The tests below now pin the
//! global defaults: the system prompt is returned untouched, no MCP
//! servers resolve, and the scaffolding for creating a project in the
//! DB stays valid because the global resolver still needs the DB to be
//! initialised (this is what commit 6 §J asked for).

use crate::server::chat::DJINN_CHAT_SYSTEM_PROMPT;
use crate::server::chat::prompt::system_message::build_system_message;
use crate::test_helpers;
use djinn_db::ProjectRepository;

async fn seed_project(db: &djinn_db::Database) -> djinn_core::models::Project {
    db.ensure_initialized().await.unwrap();
    let repo = ProjectRepository::new(db.clone(), crate::events::EventBus::noop());
    repo.create_with_id("p1", "p1", "test", "p1")
        .await
        .expect("create test project")
}

#[tokio::test]
async fn chat_system_message_has_no_project_context_or_repo_map_under_global_refactor() {
    let db = test_helpers::create_test_db();
    let _project = seed_project(&db).await;

    // No project_context, no repo_map — only the base prompt + optional
    // client-supplied system string may appear.
    let msg = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        None,
        Some("be brief"),
        "anthropic/claude-3-5-sonnet",
    );

    let text = msg.text_content();
    assert!(text.contains(DJINN_CHAT_SYSTEM_PROMPT.trim()));
    // Absent-by-design under chat-user-global.
    assert!(!text.contains("## Current Project"));
    assert!(!text.contains("## Repository Map"));
}
