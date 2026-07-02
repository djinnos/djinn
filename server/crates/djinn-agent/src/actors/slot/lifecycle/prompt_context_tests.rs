use super::*;

use djinn_core::events::EventBus;
use djinn_core::models::ActivityEntry;
use djinn_db::{Database, EpicRepository, ProposalCreateInput, ProposalRepository};
use tokio_util::sync::CancellationToken;

use crate::roles::LeadRole;
use crate::test_helpers::{agent_context_from_db, create_test_project};

use super::test_support::{
    assemble_for_role, assert_contains_all, assert_ordered, create_epic, create_project_epic_task,
    create_task,
};

async fn lead_prompt_context(db: Database, task: &Task) -> PromptContext {
    let role = LeadRole;
    assemble_for_role(db, task, &role, None, "", None, &[], &[]).await
}

async fn lead_epic_context(db: Database, task: &Task) -> String {
    lead_prompt_context(db, task)
        .await
        .epic_context
        .expect("lead prompt context includes epic context")
}

#[tokio::test]
async fn epic_context_includes_blocking_and_sibling_sections() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let epic_repo = EpicRepository::new(db.clone(), events.clone());
    let proposal_repo = ProposalRepository::new(db.clone(), events.clone());

    let subject_epic = create_epic(
        &db,
        &events,
        &project.id,
        "Subject decomposition epic",
        "Build on dependency foundations without duplicating them.",
        None,
    )
    .await;
    let blocking_epic = create_epic(
        &db,
        &events,
        &project.id,
        "Foundation blocking epic",
        "Owns the schema and migration foundation.",
        Some("closed"),
    )
    .await;
    for title in ["Ship shared migration", "Ship shared schema module"] {
        create_task(&db, &events, &blocking_epic.id, title, Some("closed")).await;
    }
    epic_repo
        .update_blockers_atomic(
            &subject_epic.id,
            std::slice::from_ref(&blocking_epic.id),
            &[],
        )
        .await
        .expect("wire epic blocker relationship");

    let sibling_epic = create_epic(
        &db,
        &events,
        &project.id,
        "Sibling proposal epic",
        "Owns a later proposal phase.",
        None,
    )
    .await;
    let proposal = proposal_repo
        .create(ProposalCreateInput {
            title: "Dependency-aware decomposition proposal",
            body: "Proposal body",
            acceptance_criteria: None,
            status: Some("building"),
            body_format: None,
        })
        .await
        .expect("create proposal");
    for epic_id in [&subject_epic.id, &sibling_epic.id] {
        proposal_repo
            .link_epic(&proposal.id, epic_id, &project.id)
            .await
            .expect("link epic to proposal");
    }

    let task = create_task(
        &db,
        &events,
        &subject_epic.id,
        "Decompose subject epic",
        None,
    )
    .await;
    let epic_context = lead_epic_context(db, &task).await;

    assert_contains_all(
        &epic_context,
        &[
            "### Blocking Epics",
            "Foundation blocking epic",
            "Ship shared migration",
            "Ship shared schema module",
            "### Proposal Sibling Epics",
            "Sibling proposal epic",
        ],
    );
}

#[tokio::test]
async fn epic_context_omits_sections_when_no_blockers_or_proposal() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Standalone epic", "Standalone task").await;

    let epic_context = lead_epic_context(db, &task).await;

    for section in ["### Blocking Epics", "### Proposal Sibling Epics"] {
        assert!(
            !epic_context.contains(section),
            "unexpected section {section}"
        );
    }
}

#[tokio::test]
async fn missing_activity_yields_none_activity_text() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "No-activity epic", "No-activity task").await;

    assert!(
        lead_prompt_context(db, &task).await.activity_text.is_none(),
        "task with no activity entries should yield None activity_text"
    );
}

#[tokio::test]
async fn conflict_context_formats_files_and_preserves_prompt_fields() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Conflict epic", "Conflict task").await;
    let conflict = MergeConflictMetadata {
        conflicting_files: vec!["src/main.rs".to_string(), "Cargo.toml".to_string()],
        base_branch: "feature-branch".to_string(),
        merge_target: "main".to_string(),
    };
    let role = LeadRole;

    let ctx = assemble_for_role(db, &task, &role, Some(&conflict), "", None, &[], &[]).await;

    assert_contains_all(
        ctx.conflict_files
            .as_deref()
            .expect("conflict_files should be Some"),
        &["- src/main.rs", "- Cargo.toml"],
    );
    assert!(!ctx.base_system_prompt.is_empty());
    assert!(!ctx.system_prompt.is_empty());
}

#[tokio::test]
async fn prompt_sections_append_in_canonical_order() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task =
        create_project_epic_task(&db, &events, "Prompt section epic", "Prompt section task").await;
    let role = LeadRole;
    let skills = vec![
        skill("alpha-skill", "First skill", "Alpha body.", true),
        skill("beta-skill", "Second skill", "Beta body.", false),
    ];
    let sources = vec![
        source("repo-a", "Repository A"),
        source("repo-b", "Repository B"),
    ];

    let ctx = assemble_for_role(
        db,
        &task,
        &role,
        None,
        "Custom extension text.",
        None,
        &skills,
        &sources,
    )
    .await;

    assert_contains_all(
        &ctx.system_prompt,
        &[
            "Custom extension text.",
            "## Available Skills",
            "**alpha-skill**",
            "**beta-skill**",
            "## Related repositories (read-only)",
            "repo-a",
            "repo-b",
        ],
    );
    assert_ordered(
        &ctx.system_prompt,
        &[
            "Custom extension text.",
            "## Available Skills",
            "## Related repositories (read-only)",
        ],
    );
}

#[test]
fn format_conflict_files_cases() {
    assert!(format_conflict_files(None).is_none());

    let ctx = MergeConflictMetadata {
        conflicting_files: vec![
            "src/main.rs".to_string(),
            "Cargo.toml".to_string(),
            "tests/integration.rs".to_string(),
        ],
        base_branch: "feature".to_string(),
        merge_target: "main".to_string(),
    };
    let result = format_conflict_files(Some(&ctx)).expect("should produce Some");
    assert_contains_all(
        &result,
        &[
            "- src/main.rs",
            "- Cargo.toml",
            "- tests/integration.rs",
            "\n",
        ],
    );

    let empty = MergeConflictMetadata {
        conflicting_files: vec![],
        base_branch: "feature".to_string(),
        merge_target: "main".to_string(),
    };
    assert_eq!(format_conflict_files(Some(&empty)).as_deref(), Some(""));
}

#[test]
fn format_activity_text_absence_and_comment_counts() {
    assert!(format_activity_text(&None, 3).is_none());
    assert!(format_activity_text(&Some(vec![]), 3).is_none());

    let entries = Some(vec![
        activity("1", "lead", "comment", r#"{"body":"Good work"}"#, 0),
        activity("2", "reviewer", "comment", r#"{"body":"Looks good"}"#, 1),
        activity("3", "lead", "comment", r#"{"body":"Approved"}"#, 2),
        activity("4", "system", "status_changed", "{}", 3),
    ]);
    let result = format_activity_text(&entries, 3).expect("should produce Some");
    assert_contains_all(&result, &["Activity totals:", "1 reviewer", "2 lead"]);
    assert!(
        !result.contains("system"),
        "non-comment events should not be counted: {result}"
    );
}

#[test]
fn apply_prompt_sections_cases() {
    assert_eq!(
        apply_prompt_sections("Base prompt.", "", None, &[], &[]),
        "Base prompt."
    );

    let result = apply_prompt_sections(
        "Base system prompt content.",
        "Custom extension text.",
        None,
        &[skill("test-skill", "A test skill", "Skill body.", false)],
        &[source("sibling-repo", "Sibling")],
    );

    assert_contains_all(
        &result,
        &[
            "Base system prompt content.",
            "Custom extension text.",
            "## Available Skills",
            "## Related repositories (read-only)",
            "sibling-repo",
        ],
    );
    assert_ordered(
        &result,
        &[
            "Custom extension text.",
            "## Available Skills",
            "## Related repositories (read-only)",
        ],
    );
}

struct NoEpicRole;

impl std::fmt::Debug for NoEpicRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoEpicRole").finish()
    }
}

impl AgentRole for NoEpicRole {
    fn config(&self) -> &crate::roles::RoleConfig {
        &NO_EPIC_CONFIG
    }

    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }

    fn needs_epic_context(&self) -> bool {
        false
    }
}

static NO_EPIC_CONFIG: crate::roles::RoleConfig = crate::roles::RoleConfig {
    name: "no-epic-test",
    display_name: "No Epic Test",
    dispatch_role: "no-epic-test",
    initial_message: "Test role without epic context.",
    finalize_tool_names: &["submit_work"],
    mode_section: None,
};

#[tokio::test]
async fn epic_context_not_loaded_when_role_does_not_need_it() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Epic", "Task").await;
    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());

    assert!(load_epic_context(&task, false, &app_state).await.is_none());

    let role = NoEpicRole;
    let ctx = assemble_for_role(db, &task, &role, None, "", None, &[], &[]).await;
    assert!(ctx.epic_context.is_none());
    assert!(ctx.knowledge_context.is_none());
}

#[tokio::test]
async fn load_epic_context_returns_context_when_epic_exists() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Test Epic Title", "Test task").await;
    let app_state = agent_context_from_db(db, CancellationToken::new());

    let result = load_epic_context(&task, true, &app_state)
        .await
        .expect("should return Some");

    assert_contains_all(
        &result,
        &[
            "Test Epic Title",
            "Test epic",
            "### Sibling Tasks",
            "memory_read",
        ],
    );
}

#[tokio::test]
async fn load_knowledge_context_returns_none_when_no_notes() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task =
        create_project_epic_task(&db, &events, "Knowledge test epic", "Knowledge task").await;
    let app_state = agent_context_from_db(db, CancellationToken::new());

    assert!(
        load_knowledge_context(&task, None, &app_state)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn load_epic_context_includes_blocker_and_sibling_sections_in_order() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let epic_repo = EpicRepository::new(db.clone(), events.clone());
    let proposal_repo = ProposalRepository::new(db.clone(), events.clone());

    let subject_epic = create_epic(
        &db,
        &events,
        &project.id,
        "Subject epic for direct helper test",
        "Direct test of load_epic_context.",
        None,
    )
    .await;
    let blocking_epic = create_epic(
        &db,
        &events,
        &project.id,
        "Blocking epic for helper test",
        "Blocks the subject.",
        Some("closed"),
    )
    .await;
    create_task(
        &db,
        &events,
        &blocking_epic.id,
        "Delivered blocker task",
        Some("closed"),
    )
    .await;
    epic_repo
        .update_blockers_atomic(
            &subject_epic.id,
            std::slice::from_ref(&blocking_epic.id),
            &[],
        )
        .await
        .expect("wire blocker");

    let sibling_epic = create_epic(
        &db,
        &events,
        &project.id,
        "Proposal sibling epic for helper test",
        "Sibling.",
        None,
    )
    .await;
    let proposal = proposal_repo
        .create(ProposalCreateInput {
            title: "Test proposal for helper",
            body: "body",
            acceptance_criteria: None,
            status: Some("building"),
            body_format: None,
        })
        .await
        .expect("create proposal");
    for epic_id in [&subject_epic.id, &sibling_epic.id] {
        proposal_repo
            .link_epic(&proposal.id, epic_id, &project.id)
            .await
            .expect("link epic");
    }

    let task = create_task(&db, &events, &subject_epic.id, "Subject task", None).await;
    let app_state = agent_context_from_db(db, CancellationToken::new());
    let result = load_epic_context(&task, true, &app_state)
        .await
        .expect("should return Some");

    assert_contains_all(
        &result,
        &[
            "### Sibling Tasks",
            "### Blocking Epics",
            "Delivered blocker task",
            "### Proposal Sibling Epics",
            "Proposal sibling epic for helper test",
        ],
    );
    assert_ordered(
        &result,
        &[
            "### Sibling Tasks",
            "### Blocking Epics",
            "### Proposal Sibling Epics",
        ],
    );
}

fn skill(name: &str, description: &str, content: &str, required: bool) -> ResolvedSkill {
    ResolvedSkill {
        name: name.to_string(),
        description: description.to_string(),
        content: content.to_string(),
        required,
        trust_level: "project".to_string(),
        recommended_for_roles: vec![],
        tags: vec![],
    }
}

fn source(slug: &str, name: &str) -> ReadSourceInfo {
    ReadSourceInfo {
        slug: slug.to_string(),
        name: name.to_string(),
    }
}

fn activity(
    id: &str,
    actor_role: &str,
    event_type: &str,
    payload: &str,
    hour: u8,
) -> ActivityEntry {
    ActivityEntry {
        id: id.to_string(),
        task_id: Some("t1".to_string()),
        actor_id: actor_role.to_string(),
        actor_role: actor_role.to_string(),
        event_type: event_type.to_string(),
        payload: payload.to_string(),
        created_at: format!("2025-01-01T0{hour}:00:00Z"),
    }
}
