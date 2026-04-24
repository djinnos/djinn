use std::path::Path;

use crate::server::chat::prompt::system_message::build_system_message;
use crate::server::chat::{apply_chat_skills, chat_effective_config};
use crate::test_helpers::workspace_tempdir;
use djinn_core::events::EventBus;
use djinn_db::{Database, ProjectRepository};
use djinn_stack::environment::EnvironmentConfig;

use super::super::DJINN_CHAT_SYSTEM_PROMPT;

fn write_skill_file(project_path: &Path, name: &str, body: &str) {
    let skills_dir = project_path.join(".djinn").join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(skills_dir.join(format!("{name}.md")), body).unwrap();
}

async fn seed_project_with_env_config(
    db: &Database,
    project_id: &str,
    project_path: &Path,
    config: &EnvironmentConfig,
) {
    db.ensure_initialized().await.unwrap();
    let repo = ProjectRepository::new(db.clone(), EventBus::noop());
    let _ = project_path; // path is now derived at runtime; retained for test compat
    repo.create_with_id(project_id, project_id, "test", project_id)
        .await
        .unwrap();
    let raw = serde_json::to_string(config).unwrap();
    repo.set_environment_config(project_id, &raw).await.unwrap();
}

#[tokio::test]
async fn chat_effective_config_uses_named_chat_defaults() {
    let dir = workspace_tempdir("chat-defaults-");
    let db = Database::open_in_memory().expect("in-memory db");
    let mut cfg = EnvironmentConfig::empty();
    cfg.agent_mcp_defaults.insert(
        "*".to_string(),
        vec!["web".to_string()],
    );
    cfg.agent_mcp_defaults.insert(
        "chat".to_string(),
        vec!["chat-web".to_string(), "chat-web".to_string()],
    );
    seed_project_with_env_config(&db, "p1", dir.path(), &cfg).await;

    let config = chat_effective_config(dir.path(), &db).await;
    assert_eq!(config.mcp_servers, vec!["chat-web"]);
}

#[tokio::test]
async fn apply_chat_skills_adds_global_skills_to_system_prompt() {
    let dir = workspace_tempdir("chat-defaults-");
    let db = Database::open_in_memory().expect("in-memory db");
    let mut cfg = EnvironmentConfig::empty();
    cfg.global_skills = vec!["git".to_string(), "rust".to_string()];
    seed_project_with_env_config(&db, "p1", dir.path(), &cfg).await;

    write_skill_file(
        dir.path(),
        "git",
        "---\ndescription: Git workflow\n---\n\nCommit cleanly.",
    );
    write_skill_file(
        dir.path(),
        "rust",
        "---\ndescription: Rust skill\n---\n\nPrefer ownership-aware fixes.",
    );

    let base = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        Some("project ctx"),
        Some("client system"),
        "anthropic/claude-3-5-sonnet",
    );

    let (message, config) = apply_chat_skills(base, Some(dir.path()), &db).await;

    assert!(message.text_content().contains("**git**: Git workflow"));
    assert!(message.text_content().contains("**rust**: Rust skill"));
    assert!(config.mcp_servers.is_empty());
}
