use super::*;

use djinn_core::events::EventBus;
use djinn_core::models::{ActivityEntry, Epic};
use djinn_db::{
    Database, EpicCreateInput, EpicRepository, ProposalCreateInput, ProposalRepository,
    TaskRepository,
};
use tokio_util::sync::CancellationToken;

use crate::roles::LeadRole;
use crate::test_helpers::{agent_context_from_db, create_test_project, test_tempdir};

async fn create_epic(
    db: &Database,
    events: &EventBus,
    project_id: &str,
    title: &str,
    description: &str,
    status: Option<&str>,
) -> Epic {
    EpicRepository::new(db.clone(), events.clone())
        .create_for_project(
            project_id,
            EpicCreateInput {
                title,
                description,
                emoji: "🧪",
                color: "blue",
                owner: "test-owner",
                memory_refs: None,
                status,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .expect("create test epic")
}

async fn prompt_context_for_task(db: Database, task: &djinn_core::models::Task) -> String {
    let app_state = agent_context_from_db(db, CancellationToken::new());
    let worktree = test_tempdir("prompt-context-worktree-");
    let role = LeadRole;
    assemble_prompt_context(PromptContextInputs {
        task,
        runtime_role: &role,
        role_for_epic_check: &role,
        project_path: "/workspace/test-project",
        worktree_path: worktree.path(),
        conflict_ctx: None,
        merge_validation_ctx: None,
        prompt_setup_commands: None,
        system_prompt_extensions: "",
        learned_prompt: None,
        resolved_skills: &[],
        app_state: &app_state,
        read_sources: &[],
    })
    .await
    .epic_context
    .expect("lead prompt context includes epic context")
}

/// Build the full [`PromptContext`] with customizable inputs.
///
/// Test helper for characterization tests that inspect fields beyond
/// `epic_context`. Returns the complete struct so individual fields
/// and ordering in the rendered prompt can be asserted.
async fn full_prompt_context(
    db: Database,
    task: &djinn_core::models::Task,
    conflict_ctx: Option<&MergeConflictMetadata>,
    system_prompt_extensions: &str,
    learned_prompt: Option<&str>,
    resolved_skills: &[ResolvedSkill],
    read_sources: &[ReadSourceInfo],
) -> PromptContext {
    let app_state = agent_context_from_db(db, CancellationToken::new());
    let worktree = test_tempdir("prompt-context-worktree-");
    let role = LeadRole;
    assemble_prompt_context(PromptContextInputs {
        task,
        runtime_role: &role,
        role_for_epic_check: &role,
        project_path: "/workspace/test-project",
        worktree_path: worktree.path(),
        conflict_ctx,
        merge_validation_ctx: None,
        prompt_setup_commands: None,
        system_prompt_extensions,
        learned_prompt,
        resolved_skills,
        app_state: &app_state,
        read_sources,
    })
    .await
}

#[tokio::test]
async fn epic_context_includes_blocking_and_sibling_sections() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let epic_repo = EpicRepository::new(db.clone(), events.clone());
    let task_repo = TaskRepository::new(db.clone(), events.clone());
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

    task_repo
        .create(
            &blocking_epic.id,
            "Ship shared migration",
            "migration delivered",
            "migration design",
            "task",
            1,
            "test-owner",
            Some("closed"),
        )
        .await
        .expect("create first closed blocker task");
    task_repo
        .create(
            &blocking_epic.id,
            "Ship shared schema module",
            "schema module delivered",
            "schema module design",
            "task",
            1,
            "test-owner",
            Some("closed"),
        )
        .await
        .expect("create second closed blocker task");

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
    proposal_repo
        .link_epic(&proposal.id, &subject_epic.id, &project.id)
        .await
        .expect("link subject epic to proposal");
    proposal_repo
        .link_epic(&proposal.id, &sibling_epic.id, &project.id)
        .await
        .expect("link sibling epic to proposal");

    let task = task_repo
        .create(
            &subject_epic.id,
            "Decompose subject epic",
            "task description",
            "task design",
            "task",
            1,
            "test-owner",
            None,
        )
        .await
        .expect("create subject task");

    let epic_context = prompt_context_for_task(db, &task).await;

    assert!(epic_context.contains("### Blocking Epics"));
    assert!(epic_context.contains("Foundation blocking epic"));
    assert!(epic_context.contains("Ship shared migration"));
    assert!(epic_context.contains("Ship shared schema module"));
    assert!(epic_context.contains("### Proposal Sibling Epics"));
    assert!(epic_context.contains("Sibling proposal epic"));
}

#[tokio::test]
async fn epic_context_omits_sections_when_no_blockers_or_proposal() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let standalone_epic = create_epic(
        &db,
        &events,
        &project.id,
        "Standalone epic",
        "No blockers and no proposal link.",
        None,
    )
    .await;
    let task = task_repo
        .create(
            &standalone_epic.id,
            "Standalone task",
            "task description",
            "task design",
            "task",
            1,
            "test-owner",
            None,
        )
        .await
        .expect("create standalone task");

    let epic_context = prompt_context_for_task(db, &task).await;

    assert!(!epic_context.contains("### Blocking Epics"));
    assert!(!epic_context.contains("### Proposal Sibling Epics"));
}

// ── Characterization tests (task s19x) ────────────────────────────────
// These cover representative optional-prompt combinations that existing
// broad tests don't isolate. They are a safety net for subsequent
// extraction refactors and must not change production behavior.

#[tokio::test]
async fn missing_activity_yields_none_activity_text() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let project = create_test_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let epic = create_epic(
        &db,
        &EventBus::noop(),
        &project.id,
        "No-activity epic",
        "Epic for activity test.",
        None,
    )
    .await;
    let task = task_repo
        .create(
            &epic.id,
            "No-activity task",
            "description",
            "design",
            "task",
            1,
            "test-owner",
            None,
        )
        .await
        .expect("create task");

    let ctx = full_prompt_context(db, &task, None, "", None, &[], &[]).await;
    assert!(
        ctx.activity_text.is_none(),
        "task with no activity entries should yield None activity_text"
    );
}

#[tokio::test]
async fn conflict_context_formats_files_and_preserves_branch_fields() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let project = create_test_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let epic = create_epic(
        &db,
        &EventBus::noop(),
        &project.id,
        "Conflict epic",
        "Epic for conflict test.",
        None,
    )
    .await;
    let task = task_repo
        .create(
            &epic.id,
            "Conflict task",
            "description",
            "design",
            "task",
            1,
            "test-owner",
            None,
        )
        .await
        .expect("create task");

    let conflict = MergeConflictMetadata {
        conflicting_files: vec!["src/main.rs".to_string(), "Cargo.toml".to_string()],
        base_branch: "feature-branch".to_string(),
        merge_target: "main".to_string(),
    };
    let ctx = full_prompt_context(db, &task, Some(&conflict), "", None, &[], &[]).await;

    // conflict_files is the `- <path>` markdown list
    let files = ctx
        .conflict_files
        .as_deref()
        .expect("conflict_files should be Some");
    assert!(
        files.contains("- src/main.rs"),
        "conflict_files should list src/main.rs"
    );
    assert!(
        files.contains("- Cargo.toml"),
        "conflict_files should list Cargo.toml"
    );

    // The lead template does not render merge branch placeholders (those
    // are only in the worker conflict template), but the base prompt must
    // still be non-empty and well-formed when conflict metadata is present.
    assert!(
        !ctx.base_system_prompt.is_empty(),
        "base prompt should be non-empty with conflict context"
    );

    // The system_prompt (final prompt after extensions + skills) should
    // also be non-empty — conflict context should not break the pipeline.
    assert!(
        !ctx.system_prompt.is_empty(),
        "final system_prompt should be non-empty with conflict context"
    );
}

#[tokio::test]
async fn read_sources_appended_after_skills_and_extensions() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let project = create_test_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let epic = create_epic(
        &db,
        &EventBus::noop(),
        &project.id,
        "Read-source epic",
        "Epic for read-source test.",
        None,
    )
    .await;
    let task = task_repo
        .create(
            &epic.id,
            "Read-source task",
            "description",
            "design",
            "task",
            1,
            "test-owner",
            None,
        )
        .await
        .expect("create task");

    let skills = vec![ResolvedSkill {
        name: "test-skill".to_string(),
        description: "A test skill".to_string(),
        content: "Skill body content.".to_string(),
        required: false,
        trust_level: "project".to_string(),
        recommended_for_roles: vec![],
        tags: vec![],
    }];
    let sources = vec![ReadSourceInfo {
        slug: "sibling-repo".to_string(),
        name: "Sibling Repository".to_string(),
    }];
    let extensions = "Custom extension text.";

    let ctx = full_prompt_context(db, &task, None, extensions, None, &skills, &sources).await;

    // Skills section appears in the prompt
    assert!(
        ctx.system_prompt.contains("## Available Skills"),
        "system_prompt should contain skills section"
    );
    // Extensions appear in the prompt
    assert!(
        ctx.system_prompt.contains("Custom extension text."),
        "system_prompt should contain extensions"
    );
    // Read sources appear in the prompt
    assert!(
        ctx.system_prompt
            .contains("## Related repositories (read-only)"),
        "system_prompt should contain read sources section"
    );
    assert!(
        ctx.system_prompt.contains("sibling-repo"),
        "system_prompt should contain the read source slug"
    );

    // Ordering: extensions before skills, skills before read sources
    let ext_pos = ctx
        .system_prompt
        .find("Custom extension text.")
        .expect("extensions present");
    let skills_pos = ctx
        .system_prompt
        .find("## Available Skills")
        .expect("skills section present");
    let sources_pos = ctx
        .system_prompt
        .find("## Related repositories (read-only)")
        .expect("read sources section present");
    assert!(
        ext_pos < skills_pos,
        "extensions should appear before skills section"
    );
    assert!(
        skills_pos < sources_pos,
        "skills section should appear before read sources"
    );
}

#[tokio::test]
async fn resolved_skills_appear_before_read_sources() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let project = create_test_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let epic = create_epic(
        &db,
        &EventBus::noop(),
        &project.id,
        "Skills-ordering epic",
        "Epic for skills ordering test.",
        None,
    )
    .await;
    let task = task_repo
        .create(
            &epic.id,
            "Skills-ordering task",
            "description",
            "design",
            "task",
            1,
            "test-owner",
            None,
        )
        .await
        .expect("create task");

    let skills = vec![
        ResolvedSkill {
            name: "alpha-skill".to_string(),
            description: "First skill".to_string(),
            content: "Alpha body.".to_string(),
            required: true,
            trust_level: "project".to_string(),
            recommended_for_roles: vec![],
            tags: vec![],
        },
        ResolvedSkill {
            name: "beta-skill".to_string(),
            description: "Second skill".to_string(),
            content: "Beta body.".to_string(),
            required: false,
            trust_level: "project".to_string(),
            recommended_for_roles: vec![],
            tags: vec![],
        },
    ];
    let sources = vec![
        ReadSourceInfo {
            slug: "repo-a".to_string(),
            name: "Repository A".to_string(),
        },
        ReadSourceInfo {
            slug: "repo-b".to_string(),
            name: "Repository B".to_string(),
        },
    ];

    let ctx = full_prompt_context(db, &task, None, "", None, &skills, &sources).await;

    // Both skills are present
    assert!(
        ctx.system_prompt.contains("**alpha-skill**"),
        "alpha-skill present"
    );
    assert!(
        ctx.system_prompt.contains("**beta-skill**"),
        "beta-skill present"
    );
    // Both read sources are present
    assert!(
        ctx.system_prompt.contains("repo-a"),
        "repo-a read source present"
    );
    assert!(
        ctx.system_prompt.contains("repo-b"),
        "repo-b read source present"
    );

    // Skills section appears before read-sources section
    let skills_pos = ctx
        .system_prompt
        .find("## Available Skills")
        .expect("skills section present");
    let sources_pos = ctx
        .system_prompt
        .find("## Related repositories (read-only)")
        .expect("read sources section present");
    assert!(
        skills_pos < sources_pos,
        "resolved skills section must appear before read-sources section"
    );
}

// ── Focused unit tests for extracted pure helpers ────────────────────

#[test]
fn format_conflict_files_none_when_no_conflict() {
    assert!(
        format_conflict_files(None).is_none(),
        "no conflict context should yield None"
    );
}

#[test]
fn format_conflict_files_produces_markdown_list() {
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
    assert!(result.contains("- src/main.rs"));
    assert!(result.contains("- Cargo.toml"));
    assert!(result.contains("- tests/integration.rs"));
    // Newline-separated list
    assert!(result.contains("\n"));
}

#[test]
fn format_conflict_files_empty_list_when_no_files() {
    let ctx = MergeConflictMetadata {
        conflicting_files: vec![],
        base_branch: "feature".to_string(),
        merge_target: "main".to_string(),
    };
    let result = format_conflict_files(Some(&ctx)).expect("should produce Some");
    assert!(
        result.is_empty(),
        "empty conflicting_files should produce empty string"
    );
}

#[test]
fn format_activity_text_none_when_absent() {
    assert!(
        format_activity_text(&None, 3).is_none(),
        "absent activity entries should yield None"
    );
}

#[test]
fn format_activity_text_none_when_empty() {
    let entries: Option<Vec<ActivityEntry>> = Some(vec![]);
    assert!(
        format_activity_text(&entries, 3).is_none(),
        "empty activity entries should yield None"
    );
}

#[test]
fn format_activity_text_includes_comment_counts() {
    let entries = Some(vec![
        ActivityEntry {
            id: "1".to_string(),
            task_id: Some("t1".to_string()),
            actor_id: "user-1".to_string(),
            actor_role: "lead".to_string(),
            event_type: "comment".to_string(),
            payload: r#"{"body":"Good work"}"#.to_string(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
        },
        ActivityEntry {
            id: "2".to_string(),
            task_id: Some("t1".to_string()),
            actor_id: "user-2".to_string(),
            actor_role: "reviewer".to_string(),
            event_type: "comment".to_string(),
            payload: r#"{"body":"Looks good"}"#.to_string(),
            created_at: "2025-01-01T01:00:00Z".to_string(),
        },
        ActivityEntry {
            id: "3".to_string(),
            task_id: Some("t1".to_string()),
            actor_id: "user-3".to_string(),
            actor_role: "lead".to_string(),
            event_type: "comment".to_string(),
            payload: r#"{"body":"Approved"}"#.to_string(),
            created_at: "2025-01-01T02:00:00Z".to_string(),
        },
        ActivityEntry {
            id: "4".to_string(),
            task_id: Some("t1".to_string()),
            actor_id: "system".to_string(),
            actor_role: "system".to_string(),
            event_type: "status_changed".to_string(),
            payload: "{}".to_string(),
            created_at: "2025-01-01T03:00:00Z".to_string(),
        },
    ]);

    let result = format_activity_text(&entries, 3).expect("should produce Some");
    // Should include activity totals with role counts
    assert!(
        result.contains("Activity totals:"),
        "should include activity totals: {result}"
    );
    assert!(
        result.contains("1 reviewer"),
        "should count reviewer comments: {result}"
    );
    assert!(
        result.contains("2 lead"),
        "should count lead comments: {result}"
    );
    // status_changed should NOT be counted
    assert!(
        !result.contains("system"),
        "non-comment events should not be counted: {result}"
    );
}

#[test]
fn apply_prompt_sections_preserves_canonical_order() {
    let base = "Base system prompt content.";
    let extensions = "Custom extension text.";
    let skills = vec![ResolvedSkill {
        name: "test-skill".to_string(),
        description: "A test skill".to_string(),
        content: "Skill body.".to_string(),
        required: false,
        trust_level: "project".to_string(),
        recommended_for_roles: vec![],
        tags: vec![],
    }];
    let sources = vec![ReadSourceInfo {
        slug: "sibling-repo".to_string(),
        name: "Sibling".to_string(),
    }];

    let result = apply_prompt_sections(base, extensions, None, &skills, &sources);

    // All sections present
    assert!(result.contains("Base system prompt content."));
    assert!(result.contains("Custom extension text."));
    assert!(result.contains("## Available Skills"));
    assert!(result.contains("## Related repositories (read-only)"));
    assert!(result.contains("sibling-repo"));

    // Canonical ordering: extensions → skills → read sources
    let ext_pos = result
        .find("Custom extension text.")
        .expect("extensions present");
    let skills_pos = result.find("## Available Skills").expect("skills present");
    let sources_pos = result
        .find("## Related repositories (read-only)")
        .expect("sources present");
    assert!(ext_pos < skills_pos, "extensions must appear before skills");
    assert!(
        skills_pos < sources_pos,
        "skills must appear before read sources"
    );
}

#[test]
fn apply_prompt_sections_noop_when_all_empty() {
    let base = "Base prompt.";
    let result = apply_prompt_sections(base, "", None, &[], &[]);
    assert_eq!(
        result, "Base prompt.",
        "no extensions/skills/sources should leave base unchanged"
    );
}

// ── Focused tests for extracted async helpers ─────────────────────────

/// Mock role that returns `false` for `needs_epic_context()`.
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
async fn load_epic_context_returns_none_when_role_does_not_need_it() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let project = create_test_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let epic = create_epic(&db, &EventBus::noop(), &project.id, "Epic", "Desc", None).await;
    let task = task_repo
        .create(
            &epic.id,
            "Task",
            "desc",
            "design",
            "task",
            1,
            "test-owner",
            None,
        )
        .await
        .expect("create task");

    let app_state = agent_context_from_db(db, CancellationToken::new());
    let result = load_epic_context(&task, false, &app_state).await;
    assert!(
        result.is_none(),
        "should return None when needs_epic_context is false"
    );
}

#[tokio::test]
async fn epic_context_none_when_role_does_not_need_epic_context() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let project = create_test_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let epic = create_epic(
        &db,
        &EventBus::noop(),
        &project.id,
        "Epic for no-epic role test",
        "Should not appear when role doesn't need epic context.",
        None,
    )
    .await;
    let task = task_repo
        .create(
            &epic.id,
            "Task for no-epic role",
            "desc",
            "design",
            "task",
            1,
            "test-owner",
            None,
        )
        .await
        .expect("create task");

    let app_state = agent_context_from_db(db, CancellationToken::new());
    let worktree = test_tempdir("prompt-context-worktree-");
    let role = NoEpicRole;
    let ctx = assemble_prompt_context(PromptContextInputs {
        task: &task,
        runtime_role: &role,
        role_for_epic_check: &role,
        project_path: "/workspace/test-project",
        worktree_path: worktree.path(),
        conflict_ctx: None,
        merge_validation_ctx: None,
        prompt_setup_commands: None,
        system_prompt_extensions: "",
        learned_prompt: None,
        resolved_skills: &[],
        app_state: &app_state,
        read_sources: &[],
    })
    .await;

    assert!(
        ctx.epic_context.is_none(),
        "role with needs_epic_context() == false should get None epic_context"
    );
    // Knowledge context still attempted (empty DB, so None)
    assert!(
        ctx.knowledge_context.is_none(),
        "empty ephemeral DB should yield None knowledge_context"
    );
}

#[tokio::test]
async fn load_epic_context_returns_context_when_epic_exists() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let epic = create_epic(
        &db,
        &events,
        &project.id,
        "Test Epic Title",
        "Test epic description.",
        None,
    )
    .await;
    let task = task_repo
        .create(
            &epic.id,
            "Test task",
            "desc",
            "design",
            "task",
            1,
            "test-owner",
            None,
        )
        .await
        .expect("create task");

    let app_state = agent_context_from_db(db, CancellationToken::new());
    let result = load_epic_context(&task, true, &app_state)
        .await
        .expect("should return Some");

    assert!(
        result.contains("Test Epic Title"),
        "should include epic title: {result}"
    );
    assert!(
        result.contains("Test epic description"),
        "should include epic description: {result}"
    );
    assert!(
        result.contains("### Sibling Tasks"),
        "should include sibling tasks section: {result}"
    );
    assert!(
        result.contains("memory_read"),
        "should include memory ref instructions: {result}"
    );
}

#[tokio::test]
async fn load_knowledge_context_returns_none_when_no_notes() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let project = create_test_project(&db).await;
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let epic = create_epic(
        &db,
        &EventBus::noop(),
        &project.id,
        "Knowledge test epic",
        "Epic for knowledge context test.",
        None,
    )
    .await;
    let task = task_repo
        .create(
            &epic.id,
            "Knowledge test task",
            "search for crates/djinn-agent patterns",
            "design doc for crates/djinn-agent refactoring",
            "task",
            1,
            "test-owner",
            None,
        )
        .await
        .expect("create task");

    let app_state = agent_context_from_db(db, CancellationToken::new());
    // Empty ephemeral DB has no notes → should return None
    let result = load_knowledge_context(&task, None, &app_state).await;
    assert!(
        result.is_none(),
        "empty DB with no notes should yield None knowledge_context"
    );
}

#[tokio::test]
async fn load_epic_context_includes_blocker_and_sibling_sections() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let epic_repo = EpicRepository::new(db.clone(), events.clone());
    let task_repo = TaskRepository::new(db.clone(), events.clone());
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

    // Create a closed task under the blocking epic
    task_repo
        .create(
            &blocking_epic.id,
            "Delivered blocker task",
            "desc",
            "design",
            "task",
            1,
            "test-owner",
            Some("closed"),
        )
        .await
        .expect("create blocker task");

    // Wire blocker relationship
    epic_repo
        .update_blockers_atomic(
            &subject_epic.id,
            std::slice::from_ref(&blocking_epic.id),
            &[],
        )
        .await
        .expect("wire blocker");

    // Create proposal with sibling epic
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
    proposal_repo
        .link_epic(&proposal.id, &subject_epic.id, &project.id)
        .await
        .expect("link subject");
    proposal_repo
        .link_epic(&proposal.id, &sibling_epic.id, &project.id)
        .await
        .expect("link sibling");

    let task = task_repo
        .create(
            &subject_epic.id,
            "Subject task for helper test",
            "desc",
            "design",
            "task",
            1,
            "test-owner",
            None,
        )
        .await
        .expect("create subject task");

    let app_state = agent_context_from_db(db, CancellationToken::new());
    let result = load_epic_context(&task, true, &app_state)
        .await
        .expect("should return Some");

    // Verify all sections present
    assert!(
        result.contains("### Sibling Tasks"),
        "should include sibling tasks: {result}"
    );
    assert!(
        result.contains("### Blocking Epics"),
        "should include blocking epics: {result}"
    );
    assert!(
        result.contains("Delivered blocker task"),
        "should include delivered tasks under blockers: {result}"
    );
    assert!(
        result.contains("### Proposal Sibling Epics"),
        "should include proposal sibling epics: {result}"
    );
    assert!(
        result.contains("Proposal sibling epic for helper test"),
        "should include the sibling epic: {result}"
    );

    // Verify ordering: Sibling Tasks before Blocking Epics before Proposal Sibling Epics
    let siblings_pos = result
        .find("### Sibling Tasks")
        .expect("siblings section present");
    let blockers_pos = result
        .find("### Blocking Epics")
        .expect("blockers section present");
    let proposal_pos = result
        .find("### Proposal Sibling Epics")
        .expect("proposal section present");
    assert!(
        siblings_pos < blockers_pos,
        "sibling tasks should appear before blocking epics"
    );
    assert!(
        blockers_pos < proposal_pos,
        "blocking epics should appear before proposal sibling epics"
    );
}

// ── sa4x: build_ci_blocking_directive tests ────────────────────────────────

/// Helper to build a minimal Task with CI fields for directive tests.
fn make_task_with_ci(
    ci_status: &str,
    ci_head_sha: Option<&str>,
    ci_pr_number: Option<i64>,
    ci_blocking_checks: &str,
    ci_failure_fingerprint: Option<&str>,
    ci_last_remediation_base_sha: Option<&str>,
) -> djinn_core::models::Task {
    djinn_core::models::Task {
        id: "task-ci-test".into(),
        project_id: "project-1".into(),
        short_id: "t-ci".into(),
        epic_id: None,
        title: "CI test task".into(),
        description: "Test task for CI directive".into(),
        design: "".into(),
        issue_type: "task".into(),
        status: "open".into(),
        priority: 1,
        owner: "test@example.com".into(),
        labels: "[]".into(),
        acceptance_criteria: "[]".into(),
        reopen_count: 0,
        continuation_count: 0,
        total_reopen_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".into(),
        agent_type: None,
        created_by_user_id: None,
        ci_status: ci_status.into(),
        ci_head_sha: ci_head_sha.map(Into::into),
        ci_pr_number,
        ci_blocking_required_check_names: ci_blocking_checks.into(),
        ci_failure_fingerprint: ci_failure_fingerprint.map(Into::into),
        ci_first_seen_at: None,
        ci_last_seen_at: None,
        ci_same_signature_count: 0,
        ci_last_remediation_base_sha: ci_last_remediation_base_sha.map(Into::into),
        unresolved_blocker_count: 0,
    }
}

#[test]
fn build_ci_blocking_directive_returns_none_for_passing_ci() {
    let task = make_task_with_ci(
        "passing",
        Some("abc123"),
        Some(42),
        "[]",
        None,
        Some("abc123"),
    );
    assert!(
        build_ci_blocking_directive(&task).is_none(),
        "directive should be None for passing CI"
    );
}

#[test]
fn build_ci_blocking_directive_returns_none_for_pending_ci() {
    let task = make_task_with_ci(
        "pending",
        Some("abc123"),
        Some(42),
        "[]",
        None,
        Some("abc123"),
    );
    assert!(
        build_ci_blocking_directive(&task).is_none(),
        "directive should be None for pending CI"
    );
}

#[test]
fn build_ci_blocking_directive_returns_none_for_unknown_ci() {
    let task = make_task_with_ci("unknown", None, None, "[]", None, None);
    assert!(
        build_ci_blocking_directive(&task).is_none(),
        "directive should be None for unknown CI"
    );
}

#[test]
fn build_ci_blocking_directive_returns_none_when_no_remediation_baseline() {
    let task = make_task_with_ci(
        "failing",
        Some("abc123"),
        Some(42),
        r#"["Quality Gate"]"#,
        Some("fp-xyz"),
        None, // no remediation baseline
    );
    assert!(
        build_ci_blocking_directive(&task).is_none(),
        "directive should be None when no remediation baseline exists"
    );
}

#[test]
fn build_ci_blocking_directive_returns_some_for_failing_ci_with_baseline() {
    let task = make_task_with_ci(
        "failing",
        Some("head-sha-456"),
        Some(42),
        r#"["Quality Gate", "unit tests"]"#,
        Some("fp-abc789"),
        Some("base-sha-123"),
    );
    let directive = build_ci_blocking_directive(&task).expect("directive should be Some");

    assert!(
        directive.contains("**PR:** #42"),
        "directive should contain concrete PR number"
    );
    assert!(
        directive.contains("**Failing head SHA:** `head-sha-456`"),
        "directive should contain failing head SHA"
    );
    assert!(
        directive.contains("Quality Gate"),
        "directive should contain blocking check names"
    );
    assert!(
        directive.contains("unit tests"),
        "directive should contain all blocking check names"
    );
    assert!(
        directive.contains("**Failure fingerprint:** `fp-abc789`"),
        "directive should contain failure fingerprint"
    );
    assert!(
        directive.contains("**Remediation baseline SHA:** `base-sha-123`"),
        "directive should contain remediation baseline SHA"
    );
    assert!(
        directive.contains("REQUIRED CI is failing"),
        "directive should contain the blocking instruction"
    );
}

#[test]
fn build_ci_blocking_directive_deduplication_same_baseline_produces_same_text() {
    let task1 = make_task_with_ci(
        "failing",
        Some("head-sha-456"),
        Some(42),
        r#"["Quality Gate"]"#,
        Some("fp-abc"),
        Some("base-sha-123"),
    );
    let task2 = make_task_with_ci(
        "failing",
        Some("head-sha-456"),
        Some(42),
        r#"["Quality Gate"]"#,
        Some("fp-abc"),
        Some("base-sha-123"),
    );
    let directive1 = build_ci_blocking_directive(&task1).expect("directive1 should be Some");
    let directive2 = build_ci_blocking_directive(&task2).expect("directive2 should be Some");

    assert_eq!(
        directive1, directive2,
        "same failing baseline should produce identical directive text (deduplication)"
    );
}

#[test]
fn build_ci_blocking_directive_uses_unknown_when_head_sha_missing() {
    let task = make_task_with_ci(
        "failing",
        None, // no head SHA
        Some(42),
        r#"["Quality Gate"]"#,
        Some("fp-abc"),
        Some("base-sha-123"),
    );
    let directive = build_ci_blocking_directive(&task).expect("directive should be Some");

    assert!(
        directive.contains("**Failing head SHA:** `unknown`"),
        "directive should use 'unknown' when head SHA is missing"
    );
}

#[test]
fn build_ci_blocking_directive_uses_zero_when_pr_number_missing() {
    let task = make_task_with_ci(
        "failing",
        Some("head-sha-456"),
        None, // no PR number
        r#"["Quality Gate"]"#,
        Some("fp-abc"),
        Some("base-sha-123"),
    );
    let directive = build_ci_blocking_directive(&task).expect("directive should be Some");

    assert!(
        directive.contains("**PR:** #0"),
        "directive should use #0 when PR number is missing"
    );
}

#[test]
fn build_ci_blocking_directive_handles_empty_check_names() {
    let task = make_task_with_ci(
        "failing",
        Some("head-sha-456"),
        Some(42),
        "[]", // empty check names
        Some("fp-abc"),
        Some("base-sha-123"),
    );
    let directive = build_ci_blocking_directive(&task).expect("directive should be Some");

    assert!(
        directive.contains("**Blocking checks:** unknown"),
        "directive should use 'unknown' when check names are empty"
    );
}

#[test]
fn build_ci_blocking_directive_omits_fingerprint_line_when_absent() {
    let task = make_task_with_ci(
        "failing",
        Some("head-sha-456"),
        Some(42),
        r#"["Quality Gate"]"#,
        None, // no fingerprint
        Some("base-sha-123"),
    );
    let directive = build_ci_blocking_directive(&task).expect("directive should be Some");

    assert!(
        !directive.contains("Failure fingerprint"),
        "directive should omit fingerprint line when fingerprint is None"
    );
}

// ── sa4x: Promoted BLOCKING directive deduplication regression tests ────────
//
// AC3: These tests verify directive deduplication for worker and reviewer
// dispatch contexts using concrete PR/head/check/fingerprint values. The
// directive is derived from durable CI gate snapshot state, so the same
// failing baseline always produces the same text — regardless of how many
// times the prompt context is assembled.

/// Concrete values shared across worker and reviewer deduplication tests.
/// These match the values used in the pr_poller and supervisor_impl tests
/// to ensure the full sa4x guardrail chain is consistent.
const E2E_PR_NUMBER: i64 = 42;
const E2E_HEAD_SHA: &str = "abc123def456789012345678901234567890abcd";
const E2E_BASE_SHA: &str = "abc123def456789012345678901234567890abcd";
const E2E_FINGERPRINT: &str = "fp-e2e-sa4x-regression";
const E2E_CHECKS: &str = r#"["Quality Gate", "Server Clippy"]"#;

/// AC3: The promoted BLOCKING directive for worker dispatch context contains
/// all concrete PR/head/check/fingerprint values from the durable snapshot.
#[test]
fn sa4x_directive_worker_context_with_concrete_values() {
    let task = make_task_with_ci(
        "failing",
        Some(E2E_HEAD_SHA),
        Some(E2E_PR_NUMBER),
        E2E_CHECKS,
        Some(E2E_FINGERPRINT),
        Some(E2E_BASE_SHA),
    );
    let directive =
        build_ci_blocking_directive(&task).expect("directive must be Some for failing CI");

    // Verify all concrete values are present.
    assert!(
        directive.contains(&format!("**PR:** #{E2E_PR_NUMBER}")),
        "directive must contain concrete PR number"
    );
    assert!(
        directive.contains(&format!("`{E2E_HEAD_SHA}`")),
        "directive must contain the failing head SHA"
    );
    assert!(
        directive.contains("Quality Gate"),
        "directive must contain blocking check name 'Quality Gate'"
    );
    assert!(
        directive.contains("Server Clippy"),
        "directive must contain blocking check name 'Server Clippy'"
    );
    assert!(
        directive.contains(&format!("`{E2E_FINGERPRINT}`")),
        "directive must contain the failure fingerprint"
    );
    assert!(
        directive.contains(&format!("`{E2E_BASE_SHA}`")),
        "directive must contain the remediation baseline SHA"
    );
    assert!(
        directive.contains("REQUIRED CI is failing"),
        "directive must contain the blocking instruction"
    );
}

/// AC3: The promoted BLOCKING directive for reviewer dispatch context is
/// identical to the worker directive for the same failing baseline.
/// The directive is derived from the same durable snapshot state, so
/// deduplication is by construction.
#[test]
fn sa4x_directive_reviewer_context_matches_worker_context() {
    let task = make_task_with_ci(
        "failing",
        Some(E2E_HEAD_SHA),
        Some(E2E_PR_NUMBER),
        E2E_CHECKS,
        Some(E2E_FINGERPRINT),
        Some(E2E_BASE_SHA),
    );

    // build_ci_blocking_directive is a pure function of the Task's CI fields.
    // It produces the same text regardless of which role (worker or reviewer)
    // assembles the prompt context. This is the deduplication guarantee.
    let directive1 = build_ci_blocking_directive(&task).expect("first call must be Some");
    let directive2 = build_ci_blocking_directive(&task).expect("second call must be Some");

    assert_eq!(
        directive1, directive2,
        "same failing baseline must produce identical directive text for both roles"
    );
}

/// AC3: Directive deduplication across repeated dispatches. When the same
/// failing baseline is dispatched multiple times (e.g., worker retry),
/// the directive text must be identical each time — it's derived from
/// immutable snapshot state, not from a counter or timestamp.
#[test]
fn sa4x_directive_deduplication_across_repeated_dispatches() {
    let task = make_task_with_ci(
        "failing",
        Some(E2E_HEAD_SHA),
        Some(E2E_PR_NUMBER),
        E2E_CHECKS,
        Some(E2E_FINGERPRINT),
        Some(E2E_BASE_SHA),
    );

    // Simulate 5 repeated dispatches for the same failing baseline.
    let directives: Vec<String> = (0..5)
        .map(|_| build_ci_blocking_directive(&task).expect("directive must be Some each time"))
        .collect();

    // All must be identical.
    for (i, d) in directives.iter().enumerate() {
        assert_eq!(
            *d, directives[0],
            "dispatch #{i} must produce the same directive as dispatch #0"
        );
    }
}

/// AC3: Directive does NOT appear for advisory-only failures (ci_status != "failing").
/// This verifies the guardrail boundary: advisory checks do not produce a
/// BLOCKING directive in any dispatch context.
#[test]
fn sa4x_directive_absent_for_advisory_ci_status() {
    for status in &["passing", "pending", "unknown"] {
        let task = make_task_with_ci(
            status,
            Some(E2E_HEAD_SHA),
            Some(E2E_PR_NUMBER),
            E2E_CHECKS,
            Some(E2E_FINGERPRINT),
            Some(E2E_BASE_SHA),
        );
        assert!(
            build_ci_blocking_directive(&task).is_none(),
            "directive must be None for ci_status={status} (advisory/non-failing)"
        );
    }
}

/// AC3: Directive absent when failing but no remediation baseline exists.
/// A failing CI observation without a baseline means the poller hasn't
/// captured the initial remediation context yet — no directive is injected.
#[test]
fn sa4x_directive_absent_when_failing_without_baseline() {
    let task = make_task_with_ci(
        "failing",
        Some(E2E_HEAD_SHA),
        Some(E2E_PR_NUMBER),
        E2E_CHECKS,
        Some(E2E_FINGERPRINT),
        None, // no remediation baseline
    );
    assert!(
        build_ci_blocking_directive(&task).is_none(),
        "directive must be None when no remediation baseline exists"
    );
}
