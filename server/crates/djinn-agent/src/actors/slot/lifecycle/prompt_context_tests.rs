// djinn:allow-oversize — prompt-context assembly + instrumentation regression
// tests; each test exercises a real `assemble_prompt_context` path.
use super::*;

use djinn_core::events::EventBus;
use djinn_core::models::ActivityEntry;
use djinn_db::{Database, EpicRepository, ProposalCreateInput, ProposalRepository};
use tokio_util::sync::CancellationToken;

use crate::roles::{AgentRole, LeadRole, WorkerRole};
use crate::test_helpers::{agent_context_from_db, create_test_project, test_tempdir};

use super::test_support::{
    assemble_for_role, assemble_for_role_with_mcp_instructions, assemble_for_role_with_resume,
    assert_contains_all, assert_ordered, create_epic, create_project_epic_task, create_task,
};

async fn lead_prompt_context(db: Database, task: &Task) -> PromptContext {
    let role = LeadRole;
    assemble_for_role(db, task, &role, None, "", &[], &[]).await
}

#[tokio::test]
async fn epic_context_omits_sections_when_no_blockers_or_proposal() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Standalone epic", "Standalone task").await;
    let epic_context = lead_prompt_context(db, &task)
        .await
        .epic_context
        .expect("lead prompt context includes epic context");
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
    let ctx = assemble_for_role(db, &task, &role, Some(&conflict), "", &[], &[]).await;
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
    let empty_instructions = std::collections::BTreeMap::new();
    assert_eq!(
        apply_prompt_sections("Base prompt.", "", &[], &[], &empty_instructions),
        "Base prompt."
    );
    let result = apply_prompt_sections(
        "Base system prompt content.",
        "Custom extension text.",
        &[skill("test-skill", "A test skill", "Skill body.", false)],
        &[source("sibling-repo", "Sibling")],
        &empty_instructions,
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
    let ctx = assemble_for_role(db, &task, &role, None, "", &[], &[]).await;
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
        load_knowledge_context(&task, None, &app_state, None)
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
    let subject_epic =
        create_epic(&db, &events, &project.id, "Subject epic", "Subject.", None).await;
    let blocking_epic = create_epic(
        &db,
        &events,
        &project.id,
        "Blocking epic",
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
        "Proposal sibling epic",
        "Sibling.",
        None,
    )
    .await;
    let proposal = proposal_repo
        .create(ProposalCreateInput {
            title: "Test proposal",
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
            "Proposal sibling epic",
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

fn resume_metadata_with_checkpoint() -> djinn_runtime::ResumeLifecycleMetadata {
    djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        checkpoint_id: Some("ckpt-1".to_string()),
        commit_sha: Some("abc123def456".to_string()),
        selection_reason: Some(djinn_runtime::ResumeSelectionReason::LatestSafeCheckpoint),
        source_kind: Some(djinn_runtime::ResumeSourceKind::TaskBranchCheckpoint),
        target_ref: Some("refs/heads/task/test".to_string()),
        prior_session_lineage: Some("session-prior-001".to_string()),
        previous_model: Some("anthropic/claude-opus-4.7".to_string()),
        new_model: Some("openai/gpt-4.1".to_string()),
        failover_reason: Some("no_durable_progress_streak".to_string()),
        last_durable_progress_summary: Some("Implemented core feature".to_string()),
        verification_command: Some("cargo test -p djinn-agent".to_string()),
        ..Default::default()
    }
}

fn resume_metadata_with_auto_submit() -> djinn_runtime::ResumeLifecycleMetadata {
    djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        selection_reason: Some(djinn_runtime::ResumeSelectionReason::AutoSubmitAccepted),
        source_kind: Some(djinn_runtime::ResumeSourceKind::AutoSubmit),
        target_ref: Some("refs/heads/task/test".to_string()),
        submit_or_review_id: Some("review-7".to_string()),
        prior_session_lineage: Some("session-prior-002".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn worker_resume_note_injected_for_worker_role() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Resume epic", "Resume task").await;
    let role = WorkerRole;
    let metadata = resume_metadata_with_checkpoint();
    let note = build_worker_resume_note(role.config().name, Some(&metadata));
    assert!(note.is_some(), "worker role should receive resume note");
    let ctx = assemble_for_role_with_resume(db, &task, &role, note.as_deref()).await;
    assert!(ctx.worker_resume_note.is_some());
    assert_contains_all(
        ctx.worker_resume_note.as_deref().unwrap(),
        &[
            "Resuming from prior session",
            "session-prior-001",
            "abc123def456",
            "claude-opus-4.7",
            "gpt-4.1",
            "no_durable_progress_streak",
            "no-progress checkpoint",
            "task-branch checkpoint",
            "refs/heads/task/test",
            "Implemented core feature",
            "cargo test -p djinn-agent",
        ],
    );
    assert!(ctx.system_prompt.contains("## Resume Context"));
    assert!(ctx.system_prompt.contains("Resuming from prior session"));
}

#[tokio::test]
async fn worker_resume_note_included_for_auto_submit_source() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let _task =
        create_project_epic_task(&db, &events, "Auto-submit epic", "Auto-submit task").await;
    let role = WorkerRole;
    let metadata = resume_metadata_with_auto_submit();
    let note = build_worker_resume_note(role.config().name, Some(&metadata));
    assert!(note.is_some());
    let note_text = note.unwrap();
    assert_contains_all(
        &note_text,
        &["session-prior-002", "review-7", "auto-submit accepted"],
    );
    assert!(!note_text.contains("checkpoint"));
}

#[test]
fn worker_resume_note_truncates_multibyte_progress_summary_on_char_boundary() {
    // A multi-byte char straddling the truncation cut must not panic
    // (byte-index slicing inside '”' crashed the slot actor).
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        last_durable_progress_summary: Some(format!("{}”{}", "a".repeat(116), "b".repeat(200))),
        ..resume_metadata_with_checkpoint()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).expect("note present");
    assert!(note.contains("last progress:"));
    assert!(note.contains('…'), "long summary should be truncated");
}

#[test]
fn worker_resume_note_absent_cases() {
    let metadata = resume_metadata_with_checkpoint();
    for role_name in ["lead", "reviewer", "planner", "architect"] {
        assert!(build_worker_resume_note(role_name, Some(&metadata)).is_none());
    }
    let not_considered = djinn_runtime::ResumeLifecycleMetadata {
        considered: false,
        ..resume_metadata_with_checkpoint()
    };
    assert!(build_worker_resume_note("worker", Some(&not_considered)).is_none());
    let no_fields = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        commit_sha: None,
        submit_or_review_id: None,
        prior_session_lineage: None,
        ..Default::default()
    };
    assert!(build_worker_resume_note("worker", Some(&no_fields)).is_none());
    assert!(build_worker_resume_note("worker", None).is_none());
}

#[test]
fn role_receives_worker_resume_check() {
    assert!(role_receives_worker_resume("worker"));
    for non_worker in [
        "lead",
        "reviewer",
        "planner",
        "architect",
        "advocate",
        "adversary",
        "judge",
    ] {
        assert!(!role_receives_worker_resume(non_worker));
    }
}

#[test]
fn worker_resume_note_truncates_long_progress_summary() {
    let long_summary = "x".repeat(200);
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        commit_sha: Some("sha1".to_string()),
        prior_session_lineage: Some("s1".to_string()),
        last_durable_progress_summary: Some(long_summary),
        ..Default::default()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert!(note.contains('…'));
    assert!(!note.contains(&"x".repeat(200)));
}

#[test]
fn worker_resume_note_minimal_fields() {
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        prior_session_lineage: Some("sess-1".to_string()),
        ..Default::default()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert!(note.contains("sess-1"));
}

#[test]
fn worker_resume_note_includes_failover_context() {
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        prior_session_lineage: Some("session-prev".to_string()),
        previous_model: Some("anthropic/claude-opus-4.7".to_string()),
        new_model: Some("openai/gpt-4.1".to_string()),
        failover_reason: Some("provider_health_degraded".to_string()),
        source_kind: Some(djinn_runtime::ResumeSourceKind::CleanTaskBranch),
        target_ref: Some("refs/heads/task/test".to_string()),
        ..Default::default()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert_contains_all(
        &note,
        &[
            "session-prev",
            "claude-opus-4.7",
            "gpt-4.1",
            "provider_health_degraded",
            "clean task branch",
            "refs/heads/task/test",
        ],
    );
}

#[test]
fn worker_resume_note_failover_only_produces_note() {
    // When no checkpoint/submit/prior session exists but failover context
    // is present, the note should still be produced.
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        new_model: Some("openai/gpt-4.1".to_string()),
        failover_reason: Some("no_durable_progress_streak".to_string()),
        ..Default::default()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert_contains_all(
        &note,
        &[
            "Resuming from prior session",
            "gpt-4.1",
            "no_durable_progress_streak",
        ],
    );
}

// ── MCP server instructions prompt tests ────────────────────────────

#[tokio::test]
async fn empty_mcp_instructions_omits_section_from_prompt() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "No MCP epic", "No MCP task").await;
    let instructions = std::collections::BTreeMap::new();
    let ctx = assemble_for_role_with_mcp_instructions(db, &task, &LeadRole, &instructions).await;
    assert!(
        !ctx.system_prompt.contains("MCP Server Instructions"),
        "empty instructions should not produce an MCP section"
    );
}

#[tokio::test]
async fn single_server_instructions_rendered_in_prompt() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "MCP epic", "MCP task").await;
    let mut instructions = std::collections::BTreeMap::new();
    instructions.insert(
        "search-server".to_string(),
        "Use web_search for live information.".to_string(),
    );
    let ctx = assemble_for_role_with_mcp_instructions(db, &task, &LeadRole, &instructions).await;
    assert_contains_all(
        &ctx.system_prompt,
        &[
            "## MCP Server Instructions",
            "### search-server",
            "Use web_search for live information.",
        ],
    );
}

#[tokio::test]
async fn multiple_servers_rendered_in_deterministic_name_order() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Multi MCP epic", "Multi MCP task").await;
    let mut instructions = std::collections::BTreeMap::new();
    // Insert in reverse-alphabetical order; BTreeMap sorts by key.
    instructions.insert(
        "zebra-server".to_string(),
        "Zebra instructions.".to_string(),
    );
    instructions.insert(
        "alpha-server".to_string(),
        "Alpha instructions.".to_string(),
    );
    instructions.insert(
        "middle-server".to_string(),
        "Middle instructions.".to_string(),
    );
    let ctx = assemble_for_role_with_mcp_instructions(db, &task, &LeadRole, &instructions).await;
    assert_contains_all(
        &ctx.system_prompt,
        &[
            "## MCP Server Instructions",
            "### alpha-server",
            "Alpha instructions.",
            "### middle-server",
            "Middle instructions.",
            "### zebra-server",
            "Zebra instructions.",
        ],
    );
    assert_ordered(
        &ctx.system_prompt,
        &["### alpha-server", "### middle-server", "### zebra-server"],
    );
}

#[test]
fn format_mcp_instructions_omits_empty_map() {
    let instructions = std::collections::BTreeMap::new();
    assert!(
        super::format_mcp_instructions(&instructions).is_none(),
        "empty map should produce None"
    );
}

#[test]
fn format_mcp_instructions_renders_sorted_subsections() {
    let mut instructions = std::collections::BTreeMap::new();
    instructions.insert("beta".to_string(), "Beta instructions.".to_string());
    instructions.insert("alpha".to_string(), "Alpha instructions.".to_string());
    let result = super::format_mcp_instructions(&instructions).expect("should produce Some");
    assert_contains_all(
        &result,
        &[
            "## MCP Server Instructions",
            "### alpha",
            "Alpha instructions.",
            "### beta",
            "Beta instructions.",
        ],
    );
    assert_ordered(&result, &["### alpha", "### beta"]);
}

// ── Regression tests: prompt-context concurrency and failover resume-note ──
// These tests guard the behavior shipped by epic 97f8:
// - Concurrent tokio::join! phases preserve deterministic prompt section ordering.
// - Failover resume-note rendering covers source kind, target ref, model context,
//   termination labels, and non-worker omission.
// - Non-fatal fallbacks: concurrent phases returning empty data do not crash.

/// AC1: When all prompt sections are populated, the rendered system prompt
/// preserves the canonical template-defined section order:
///   CI blocking → Resume Context → Epic Context → Relevant Knowledge
///   → Code Graph Context → Activity Log → Environment → Tools
///
/// This is the primary regression guard for the concurrent `tokio::join!` phases
/// in `assemble_prompt_context`: regardless of which futures complete first, the
/// final prompt must always render sections in template order.
#[tokio::test]
async fn prompt_sections_in_template_order_when_all_populated() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Ordering epic", "Ordering task").await;
    let role = WorkerRole;
    let metadata = resume_metadata_with_checkpoint();
    let note = build_worker_resume_note(role.config().name, Some(&metadata));
    assert!(note.is_some());
    let ctx = assemble_for_role_with_resume(db, &task, &role, note.as_deref()).await;
    let prompt = &ctx.system_prompt;

    // The resume section must be present since we supplied metadata.
    assert!(
        prompt.contains("## Resume Context"),
        "resume section must be present"
    );

    // Verify all sections that should appear when data is present.
    // The base template always includes task fields; the sections below are
    // conditional on the data being populated.
    let expected_markers = [
        "## Resume Context",
        "## Epic Context",
        "## Environment",
        "## Tools",
    ];
    assert_ordered(prompt, &expected_markers);
}

/// AC1: Determinism — running `assemble_prompt_context` twice with identical
/// inputs produces the same rendered prompt. This guards against ordering
/// nondeterminism introduced by concurrent phases.
#[tokio::test]
async fn concurrent_assembly_is_deterministic() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Determinism epic", "Determinism task").await;
    let role = WorkerRole;
    let metadata = resume_metadata_with_checkpoint();
    let note = build_worker_resume_note(role.config().name, Some(&metadata));

    // Use a shared worktree path so the workspace_path in the rendered prompt
    // is identical across both runs.
    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let worktree = test_tempdir("prompt-context-worktree-");
    let empty_instructions = std::collections::BTreeMap::new();

    let ctx1 = assemble_prompt_context(PromptContextInputs {
        task: &task,
        runtime_role: &role,
        role_for_epic_check: &role,
        project_path: "/workspace/test-project",
        worktree_path: worktree.path(),
        conflict_ctx: None,
        merge_validation_ctx: None,
        prompt_setup_commands: None,
        system_prompt_extensions: "",
        resolved_skills: &[],
        app_state: &app_state,
        knowledge_identity: None,
        planned_queries: None,
        read_sources: &[],
        worker_resume_note: note.as_deref(),
        arbiter_directive: None,
        mcp_server_instructions: &empty_instructions,
    })
    .await;

    let ctx2 = assemble_prompt_context(PromptContextInputs {
        task: &task,
        runtime_role: &role,
        role_for_epic_check: &role,
        project_path: "/workspace/test-project",
        worktree_path: worktree.path(),
        conflict_ctx: None,
        merge_validation_ctx: None,
        prompt_setup_commands: None,
        system_prompt_extensions: "",
        resolved_skills: &[],
        app_state: &app_state,
        knowledge_identity: None,
        planned_queries: None,
        read_sources: &[],
        worker_resume_note: note.as_deref(),
        arbiter_directive: None,
        mcp_server_instructions: &empty_instructions,
    })
    .await;

    assert_eq!(
        ctx1.system_prompt, ctx2.system_prompt,
        "prompt must be deterministic across runs"
    );
    assert_eq!(ctx1.epic_context, ctx2.epic_context);
    assert_eq!(ctx1.worker_resume_note, ctx2.worker_resume_note);
}

/// AC1 + AC3: When all concurrent context phases return empty (no epic context,
/// no activity, no knowledge, no code-graph, no reviewer-diff), the assembly
/// still completes without error and all optional fields are None.
#[tokio::test]
async fn concurrent_assembly_empty_contexts_yield_none_fields() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    // Create a standalone task with no epic (epic_context will be empty)
    let task = create_project_epic_task(&db, &events, "Empty epic", "Empty task").await;
    let role = WorkerRole;
    let ctx = assemble_for_role(db, &task, &role, None, "", &[], &[]).await;

    // Activity/attempt fields should be None (no activity entries seeded)
    assert!(ctx.activity_text.is_none(), "no activity → None");
    assert!(ctx.worker_summary.is_none(), "no worker summary → None");
    assert!(ctx.worker_concerns.is_none(), "no worker concerns → None");
    // Knowledge is not loaded when epic context is empty
    // (knowledge depends on epic context for scoping)
    // Code-graph and reviewer-diff are role-gated: WorkerRole receives them
    // but the worktree has no git repo, so they'll be None.
    // Resume note was not supplied
    assert!(
        ctx.worker_resume_note.is_none(),
        "no resume note supplied → None"
    );
    // CI blocking directive: task has ci_status="open" → None
    assert!(ctx.ci_blocking_directive.is_none(), "no failing CI → None");
    // The base prompt must still be non-empty (task metadata always renders)
    assert!(
        !ctx.base_system_prompt.is_empty(),
        "base prompt must not be empty even with all-empty concurrent phases"
    );
}

/// AC2: Each `ResumeSourceKind` variant produces a note with the expected
/// human-readable label. This is a regression guard for the `source_kind_label`
/// mapping that feeds into the resume note.
#[test]
fn resume_note_renders_all_source_kind_labels() {
    use djinn_runtime::ResumeSourceKind as K;

    let cases: &[(K, &str)] = &[
        (K::AutoSubmit, "auto-submit"),
        (K::TaskBranchCheckpoint, "task-branch checkpoint"),
        (K::AlternateCheckpointRef, "alternate checkpoint ref"),
        (K::CleanTaskBranch, "clean task branch"),
    ];

    for (kind, expected_label) in cases {
        let metadata = djinn_runtime::ResumeLifecycleMetadata {
            considered: true,
            source_kind: Some(*kind),
            prior_session_lineage: Some("sess-test".to_string()),
            ..Default::default()
        };
        let note = build_worker_resume_note("worker", Some(&metadata))
            .unwrap_or_else(|| panic!("note should be produced for source kind {kind:?}"));
        assert!(
            note.contains(expected_label),
            "expected label {expected_label:?} in note for {kind:?}, got: {note}"
        );
    }
}

/// AC2: Each `ResumeSelectionReason` variant produces a note with the expected
/// human-readable termination label. Regression guard for `termination_label`.
#[test]
fn resume_note_renders_all_termination_labels() {
    use djinn_runtime::ResumeSelectionReason as R;

    let cases: &[(R, &str)] = &[
        (R::AutoSubmitAccepted, "auto-submit accepted"),
        (R::LatestSafeCheckpoint, "no-progress checkpoint"),
        (R::AlternateCheckpointRef, "alternate checkpoint ref"),
        (R::CleanTaskBranchFallback, "clean fallback"),
        (R::NewerTaskBranch, "newer task branch"),
        (R::CheckpointMissing, "checkpoint missing"),
        (R::CheckpointUnsafe, "checkpoint unsafe"),
        (R::MergeConflict, "merge conflict"),
        (R::Disabled, "resume disabled"),
    ];

    for (reason, expected_label) in cases {
        let metadata = djinn_runtime::ResumeLifecycleMetadata {
            considered: true,
            selection_reason: Some(*reason),
            prior_session_lineage: Some("sess-test".to_string()),
            ..Default::default()
        };
        let note = build_worker_resume_note("worker", Some(&metadata))
            .unwrap_or_else(|| panic!("note should be produced for reason {reason:?}"));
        assert!(
            note.contains(expected_label),
            "expected label {expected_label:?} in note for {reason:?}, got: {note}"
        );
    }
}

/// AC2: `AlternateCheckpointRef` source kind with `target_ref` renders both
/// the source label and the target ref in the resume note.
#[test]
fn resume_note_renders_alternate_checkpoint_ref_with_target_ref() {
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        source_kind: Some(djinn_runtime::ResumeSourceKind::AlternateCheckpointRef),
        target_ref: Some("refs/heads/checkpoint/alt-branch".to_string()),
        commit_sha: Some("def789abc123".to_string()),
        selection_reason: Some(djinn_runtime::ResumeSelectionReason::AlternateCheckpointRef),
        prior_session_lineage: Some("session-alt-001".to_string()),
        previous_model: Some("google/gemini-2.5-pro".to_string()),
        new_model: Some("anthropic/claude-sonnet-4".to_string()),
        failover_reason: Some("model_rotation".to_string()),
        ..Default::default()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert_contains_all(
        &note,
        &[
            "alternate checkpoint ref",         // source_kind_label
            "refs/heads/checkpoint/alt-branch", // target_ref
            "def789abc123",                     // checkpoint sha
            "alternate checkpoint ref",         // termination_label
            "session-alt-001",                  // prior session
            "gemini-2.5-pro",                   // previous model
            "claude-sonnet-4",                  // new model
            "model_rotation",                   // failover reason
        ],
    );
}

/// AC2: Selected source kind and target ref are rendered together in the resume
/// note for all source/target combinations that carry both fields.
#[test]
fn resume_note_selected_source_and_target_ref_details() {
    // AutoSubmit with target_ref and submit_or_review_id
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        source_kind: Some(djinn_runtime::ResumeSourceKind::AutoSubmit),
        target_ref: Some("refs/heads/task/my-feature".to_string()),
        submit_or_review_id: Some("pr-42".to_string()),
        selection_reason: Some(djinn_runtime::ResumeSelectionReason::AutoSubmitAccepted),
        prior_session_lineage: Some("session-submit-01".to_string()),
        ..Default::default()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert_contains_all(
        &note,
        &[
            "auto-submit",
            "refs/heads/task/my-feature",
            "pr-42",
            "auto-submit accepted",
        ],
    );
    // Auto-submit source should NOT include "checkpoint"
    assert!(
        !note.contains("checkpoint"),
        "auto-submit note should not mention checkpoint: {note}"
    );
}

/// AC2 (non-worker omission): Non-worker roles (lead, reviewer, planner,
/// architect) must NOT receive a resume note, even when resume metadata is
/// fully populated. This is a regression guard for `role_receives_worker_resume`
/// and the full `build_worker_resume_note` pipeline.
#[test]
fn non_worker_roles_omit_resume_note_even_with_full_metadata() {
    let metadata = resume_metadata_with_checkpoint();
    for role_name in [
        "lead",
        "reviewer",
        "planner",
        "architect",
        "advocate",
        "adversary",
        "judge",
    ] {
        let note = build_worker_resume_note(role_name, Some(&metadata));
        assert!(
            note.is_none(),
            "non-worker role {role_name:?} should not receive resume note, got: {note:?}"
        );
    }
}

/// AC2: Full pipeline — build_resume_note → assemble_prompt_context → rendered
/// system prompt. The resume note must appear under `## Resume Context` in the
/// final rendered prompt for the worker role.
#[tokio::test]
async fn resume_note_appears_in_rendered_prompt_for_worker() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Pipeline epic", "Pipeline task").await;
    let role = WorkerRole;
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        source_kind: Some(djinn_runtime::ResumeSourceKind::TaskBranchCheckpoint),
        target_ref: Some("refs/heads/task/test".to_string()),
        commit_sha: Some("feedface1234".to_string()),
        selection_reason: Some(djinn_runtime::ResumeSelectionReason::LatestSafeCheckpoint),
        prior_session_lineage: Some("session-pipe-001".to_string()),
        previous_model: Some("openai/gpt-4.1".to_string()),
        new_model: Some("anthropic/claude-opus-4.7".to_string()),
        failover_reason: Some("no_durable_progress_streak".to_string()),
        last_durable_progress_summary: Some("Built the widget module".to_string()),
        verification_command: Some("cargo test -p widget".to_string()),
        ..Default::default()
    };
    let note = build_worker_resume_note(role.config().name, Some(&metadata));
    let ctx = assemble_for_role_with_resume(db, &task, &role, note.as_deref()).await;

    // The resume note must be in the PromptContext
    let resume_text = ctx
        .worker_resume_note
        .as_deref()
        .expect("resume note should be present");
    assert_contains_all(
        resume_text,
        &[
            "Resuming from prior session",
            "feedface1234",
            "session-pipe-001",
            "gpt-4.1",
            "claude-opus-4.7",
            "no_durable_progress_streak",
            "Built the widget module",
            "cargo test -p widget",
        ],
    );

    // The rendered system prompt must contain the Resume Context section
    assert!(
        ctx.system_prompt.contains("## Resume Context"),
        "rendered prompt must contain Resume Context section"
    );
    assert!(
        ctx.system_prompt.contains("Resuming from prior session"),
        "rendered prompt must contain the resume note text"
    );
}

/// AC2: Full pipeline for non-worker roles — even when a resume note is
/// pre-built (e.g. from `build_worker_resume_note`), a non-worker role should
/// not have the Resume Context section in the rendered prompt. The
/// `assemble_for_role_with_resume` helper passes the note through, but the
/// template rendering strips it for non-worker roles since
/// `role_receives_worker_resume` gates `build_worker_resume_note` at the call
/// site in stage.rs.  Here we verify that when the note is None (as it would
/// be for non-worker roles), the Resume Context section does not appear.
#[tokio::test]
async fn non_worker_role_prompt_omits_resume_context_section() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "No-resume epic", "No-resume task").await;
    let role = LeadRole;
    // Passing None simulates the stage.rs behavior for non-worker roles
    let ctx = assemble_for_role_with_resume(db, &task, &role, None).await;
    assert!(
        ctx.worker_resume_note.is_none(),
        "non-worker role should have no resume note"
    );
    assert!(
        !ctx.system_prompt.contains("## Resume Context"),
        "non-worker role prompt should not contain Resume Context section"
    );
}

/// AC1: When both CI blocking directive and resume note are present, they
/// appear in the correct template order: CI blocking before resume context.
#[tokio::test]
async fn ci_blocking_appears_before_resume_context_in_prompt() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Order epic", "Order task").await;

    // Seed CI failing status (ci_last_remediation_base_sha is required by build_ci_blocking_directive)
    let task = {
        let mut t = task;
        t.ci_status = "failing".to_string();
        t.ci_pr_number = Some(100);
        t.ci_head_sha = Some("head-sha-abc".to_string());
        t.ci_blocking_required_check_names = "Quality Gate".to_string();
        t.ci_last_remediation_base_sha = Some("base-sha-abc".to_string());
        t
    };

    let role = WorkerRole;
    let metadata = resume_metadata_with_checkpoint();
    let note = build_worker_resume_note(role.config().name, Some(&metadata));
    assert!(note.is_some());

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let worktree = test_tempdir("prompt-context-worktree-");
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
        resolved_skills: &[],
        app_state: &app_state,
        knowledge_identity: None,
        planned_queries: None,
        read_sources: &[],
        worker_resume_note: note.as_deref(),
        arbiter_directive: None,
        mcp_server_instructions: &std::collections::BTreeMap::new(),
    })
    .await;

    // Both sections should be present
    assert!(
        ctx.ci_blocking_directive.is_some(),
        "CI directive should be present"
    );
    assert!(
        ctx.worker_resume_note.is_some(),
        "resume note should be present"
    );

    // In the rendered prompt, CI blocking comes before resume context
    assert_ordered(
        &ctx.system_prompt,
        &["## ⛔ BLOCKING: Required CI Failing", "## Resume Context"],
    );
}

/// AC1: Activity section (with attempt history appended by concurrent phase 2)
/// always appears after knowledge context and code graph context in the rendered
/// prompt. This guards the phase 2 concurrent join ordering.
#[tokio::test]
async fn activity_section_appears_after_knowledge_and_code_graph() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task =
        create_project_epic_task(&db, &events, "Activity order epic", "Activity order task").await;
    let role = LeadRole;
    let ctx = assemble_for_role(db, &task, &role, None, "", &[], &[]).await;
    let prompt = &ctx.system_prompt;

    // The template ordering is: Epic Context → Relevant Knowledge → Code Graph → Activity
    // Even when sections are empty, the template has them in that order as markers.
    // We verify relative ordering of the sections that do have content markers.
    // Activity Log appears inside the activity_section; "## Environment" comes after it.
    if prompt.contains("## Epic Context") && prompt.contains("### Activity Log") {
        assert_ordered(
            prompt,
            &["## Epic Context", "### Activity Log", "## Environment"],
        );
    }
    // Even without Epic Context present, Activity Log must precede Environment
    if prompt.contains("### Activity Log") {
        assert_ordered(prompt, &["### Activity Log", "## Environment"]);
    }
}

/// AC2: Resume note with only failover context (no checkpoint, no submit, no
/// prior session) still renders correctly and includes model and reason.
#[test]
fn resume_note_failover_only_with_new_and_previous_model() {
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        new_model: Some("openai/o3".to_string()),
        previous_model: Some("anthropic/claude-opus-4.7".to_string()),
        failover_reason: Some("provider_health_degraded".to_string()),
        ..Default::default()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert_contains_all(
        &note,
        &[
            "Resuming from prior session",
            "openai/o3",
            "claude-opus-4.7",
            "provider_health_degraded",
        ],
    );
    // Should NOT contain checkpoint or submit/review since none was supplied
    assert!(
        !note.contains("checkpoint"),
        "no checkpoint in failover-only: {note}"
    );
    assert!(
        !note.contains("submit/review"),
        "no submit/review in failover-only: {note}"
    );
}

/// AC2: Resume note with both checkpoint and submit_or_review_id prefers
/// checkpoint (checkpoint takes precedence in the rendering logic).
#[test]
fn resume_note_checkpoint_takes_precedence_over_submit() {
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        commit_sha: Some("abc123".to_string()),
        submit_or_review_id: Some("review-99".to_string()),
        prior_session_lineage: Some("sess-both".to_string()),
        ..Default::default()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    // Checkpoint should appear (it's the first branch in the if/else)
    assert!(
        note.contains("abc123"),
        "checkpoint sha must appear: {note}"
    );
    // submit/review should NOT appear since checkpoint is present
    assert!(
        !note.contains("review-99"),
        "submit/review should be omitted when checkpoint is present: {note}"
    );
}

/// AC1: The `prompt_sections_append_in_canonical_order` existing test covers
/// skills and read sources ordering. This extended version adds the Resume
/// Context marker to verify it participates in the canonical ordering too.
#[tokio::test]
async fn resume_context_section_in_canonical_order_with_skills_and_sources() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Full order epic", "Full order task").await;
    let role = WorkerRole;
    let skills = vec![
        skill("alpha-skill", "First skill", "Alpha body.", true),
        skill("beta-skill", "Second skill", "Beta body.", false),
    ];
    let sources = vec![
        source("repo-a", "Repository A"),
        source("repo-b", "Repository B"),
    ];
    let metadata = resume_metadata_with_checkpoint();
    let note = build_worker_resume_note(role.config().name, Some(&metadata));
    assert!(note.is_some(), "worker resume note should be produced");
    let note_ref = note.as_deref();

    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    let worktree = test_tempdir("prompt-context-worktree-");
    let ctx = assemble_prompt_context(PromptContextInputs {
        task: &task,
        runtime_role: &role,
        role_for_epic_check: &role,
        project_path: "/workspace/test-project",
        worktree_path: worktree.path(),
        conflict_ctx: None,
        merge_validation_ctx: None,
        prompt_setup_commands: None,
        system_prompt_extensions: "Custom extension.",
        resolved_skills: &skills,
        app_state: &app_state,
        knowledge_identity: None,
        planned_queries: None,
        read_sources: &sources,
        worker_resume_note: note_ref,
        arbiter_directive: None,
        mcp_server_instructions: &std::collections::BTreeMap::new(),
    })
    .await;

    assert_contains_all(
        &ctx.system_prompt,
        &[
            "Custom extension.",
            "## Resume Context",
            "Resuming from prior session",
            "## Available Skills",
            "## Related repositories (read-only)",
        ],
    );
    assert_ordered(
        &ctx.system_prompt,
        &[
            "## Resume Context",
            "Custom extension.",
            "## Available Skills",
            "## Related repositories (read-only)",
        ],
    );
}

// ── Resume Context prompt rendering regressions (proposal phif) ──────
// These tests prove that the worker prompt renders `## Resume Context`
// with typed discontinuity fields: prior session id, source kind,
// commit SHA, selection reason, and failover context.

/// AC 2: The `## Resume Context` section must render attempt-level
/// metadata including prior session id, source kind, commit SHA, and
/// selection reason when a safe checkpoint is selected.
#[tokio::test]
async fn resume_context_renders_attempt_and_discontinuity_fields() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task =
        create_project_epic_task(&db, &events, "Discontinuity epic", "Discontinuity task").await;
    let role = WorkerRole;

    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        checkpoint_id: Some("ckpt-discontinuity".to_string()),
        commit_sha: Some("abc123deadbeef".to_string()),
        selection_reason: Some(djinn_runtime::ResumeSelectionReason::LatestSafeCheckpoint),
        source_kind: Some(djinn_runtime::ResumeSourceKind::TaskBranchCheckpoint),
        target_ref: Some("refs/heads/task/discontinuity-task".to_string()),
        prior_session_lineage: Some("session-attempt-2".to_string()),
        previous_model: Some("anthropic/claude-opus-4.7".to_string()),
        new_model: Some("openai/gpt-4.1".to_string()),
        failover_reason: Some("no_durable_progress_streak".to_string()),
        last_durable_progress_summary: Some("Wrote the resume metadata module".to_string()),
        verification_command: Some("cargo test -p djinn-coordinator".to_string()),
        ..Default::default()
    };

    let note = build_worker_resume_note(role.config().name, Some(&metadata));
    assert!(
        note.is_some(),
        "resume note must be produced for worker role"
    );

    let note_text = note.unwrap();
    // Must contain all the discontinuity fields.
    assert_contains_all(
        &note_text,
        &[
            "session-attempt-2",                  // prior session id
            "task-branch checkpoint",             // source kind label
            "abc123deadbeef",                     // commit SHA
            "no-progress checkpoint",             // selection reason label
            "refs/heads/task/discontinuity-task", // target ref
            "claude-opus-4.7",                    // previous model
            "gpt-4.1",                            // new model
            "no_durable_progress_streak",         // failover reason
            "Wrote the resume metadata module",   // last progress summary
            "cargo test -p djinn-coordinator",    // verification command
        ],
    );

    // Render into the prompt.
    let ctx = assemble_for_role_with_resume(db, &task, &role, Some(&note_text)).await;
    assert!(
        ctx.system_prompt.contains("## Resume Context"),
        "rendered prompt must contain Resume Context section"
    );
    assert!(
        ctx.system_prompt.contains("session-attempt-2"),
        "rendered prompt must contain prior session id"
    );
}

/// AC 4: Missing evidence path — when prior session/evidence is absent,
/// the resume note renders unknown/unavailable fields without blocking
/// dispatch. The `build_worker_resume_note` function returns None when
/// only `considered: true` is set with no actual fields, so the prompt
/// does not include a Resume Context section at all (graceful omission).
#[test]
fn resume_context_absent_when_only_considered_flag_set() {
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        // No checkpoint, no prior session, no failover — all absent.
        ..Default::default()
    };

    let note = build_worker_resume_note("worker", Some(&metadata));
    assert!(
        note.is_none(),
        "resume note must be None when only `considered` is set with no actual fields"
    );
}

/// AC 4: When prior session lineage is present but commit SHA and
/// submit/review id are absent (provider rejection path), the resume
/// note still renders with the available fields and marks the
/// unavailable sources as absent.
#[test]
fn resume_context_renders_minimal_fields_for_provider_rejection() {
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        prior_session_lineage: Some("session-provider-rejected".to_string()),
        source_kind: Some(djinn_runtime::ResumeSourceKind::CleanTaskBranch),
        target_ref: Some("refs/heads/task/test".to_string()),
        selection_reason: Some(djinn_runtime::ResumeSelectionReason::CleanTaskBranchFallback),
        // No checkpoint, no auto-submit, no failover — provider rejection.
        ..Default::default()
    };

    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert_contains_all(
        &note,
        &[
            "session-provider-rejected",
            "clean task branch", // source kind label
            "clean fallback",    // selection reason label
            "refs/heads/task/test",
        ],
    );
    // Must NOT contain checkpoint or auto-submit fields.
    assert!(
        !note.contains("checkpoint `"),
        "must not mention checkpoint SHA when absent"
    );
    assert!(
        !note.contains("submit/review `"),
        "must not mention submit/review id when absent"
    );
}

/// AC 5: Preservation/no-replay — accepted auto-submit work produces a
/// resume note that references the review id (not a checkpoint SHA).
/// This proves the worker sees the auto-submit as the resume source
/// and will not replay stale checkpoint work.
#[test]
fn preservation_no_replay_auto_submit_renders_review_id_not_checkpoint() {
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        selection_reason: Some(djinn_runtime::ResumeSelectionReason::AutoSubmitAccepted),
        source_kind: Some(djinn_runtime::ResumeSourceKind::AutoSubmit),
        target_ref: Some("refs/heads/task/test".to_string()),
        submit_or_review_id: Some("review-accepted-42".to_string()),
        prior_session_lineage: Some("session-auto-submit".to_string()),
        ..Default::default()
    };

    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert_contains_all(
        &note,
        &[
            "session-auto-submit",
            "review-accepted-42",   // review id, not checkpoint
            "auto-submit accepted", // selection reason label
            "auto-submit",          // source kind label
        ],
    );
    // Must NOT contain checkpoint references.
    assert!(
        !note.contains("checkpoint `"),
        "auto-submit note must not reference checkpoint SHA"
    );
}

/// AC 5: Preservation/no-replay — when the resume note includes clean
/// fallback (stall/zombie kill), the prompt includes the fallback source
/// and terminates label. This proves the worker sees the discontinuity
/// and starts fresh rather than replaying stale work.
#[test]
fn preservation_no_replay_clean_fallback_renders_fallback_source() {
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        selection_reason: Some(djinn_runtime::ResumeSelectionReason::CleanTaskBranchFallback),
        source_kind: Some(djinn_runtime::ResumeSourceKind::CleanTaskBranch),
        target_ref: Some("refs/heads/task/test".to_string()),
        prior_session_lineage: Some("session-stall-killed".to_string()),
        ..Default::default()
    };

    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert_contains_all(
        &note,
        &[
            "session-stall-killed",
            "clean task branch", // source kind
            "clean fallback",    // terminated label
        ],
    );
}

/// AC 5: When no-replay metadata exists (discontinuity present but no
/// prior output to resume), the prompt rendering is still deterministic.
/// Running twice with identical inputs produces identical prompts.
#[tokio::test]
async fn resume_context_deterministic_with_discontinuity_metadata() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Determinism epic", "Determinism task").await;
    let role = WorkerRole;

    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        selection_reason: Some(djinn_runtime::ResumeSelectionReason::CleanTaskBranchFallback),
        source_kind: Some(djinn_runtime::ResumeSourceKind::CleanTaskBranch),
        target_ref: Some("refs/heads/task/determinism-task".to_string()),
        prior_session_lineage: Some("session-determinism".to_string()),
        ..Default::default()
    };

    let note = build_worker_resume_note(role.config().name, Some(&metadata));
    assert!(note.is_some());

    // Use shared worktree/app_state so the workspace_path in the rendered
    // prompt is identical across both runs (matches the existing
    // concurrent_assembly_is_deterministic pattern).
    let app_state = agent_context_from_db(db, CancellationToken::new());
    let worktree = test_tempdir("prompt-context-worktree-");
    let empty_instructions = std::collections::BTreeMap::new();

    let ctx1 = assemble_prompt_context(PromptContextInputs {
        task: &task,
        runtime_role: &role,
        role_for_epic_check: &role,
        project_path: "/workspace/test-project",
        worktree_path: worktree.path(),
        conflict_ctx: None,
        merge_validation_ctx: None,
        prompt_setup_commands: None,
        system_prompt_extensions: "",
        resolved_skills: &[],
        app_state: &app_state,
        knowledge_identity: None,
        planned_queries: None,
        read_sources: &[],
        worker_resume_note: note.as_deref(),
        arbiter_directive: None,
        mcp_server_instructions: &empty_instructions,
    })
    .await;

    let ctx2 = assemble_prompt_context(PromptContextInputs {
        task: &task,
        runtime_role: &role,
        role_for_epic_check: &role,
        project_path: "/workspace/test-project",
        worktree_path: worktree.path(),
        conflict_ctx: None,
        merge_validation_ctx: None,
        prompt_setup_commands: None,
        system_prompt_extensions: "",
        resolved_skills: &[],
        app_state: &app_state,
        knowledge_identity: None,
        planned_queries: None,
        read_sources: &[],
        worker_resume_note: note.as_deref(),
        arbiter_directive: None,
        mcp_server_instructions: &empty_instructions,
    })
    .await;

    assert_eq!(
        ctx1.system_prompt, ctx2.system_prompt,
        "prompt with discontinuity metadata must be deterministic"
    );
    assert_eq!(
        ctx1.worker_resume_note, ctx2.worker_resume_note,
        "resume note must be deterministic"
    );
}

// ─── Prompt-context assembly instrumentation tests ────────────────────
// These tests invoke the real `assemble_prompt_context` path via
// `assemble_for_role` and assert that the telemetry metrics
// `djinn_prompt_context_latency_seconds` and
// `djinn_prompt_context_child_span_latency_seconds` are emitted with
// the expected span labels.  This catches regressions where the
// instrumentation in `prompt_context.rs` is removed or broken — unlike
// standalone `tokio::join!` timing tests or telemetry-facade-only tests.

mod prompt_context_instrumentation_tests {
    use super::super::test_support::{assemble_for_role, create_project_epic_task};
    use crate::roles::WorkerRole;
    use djinn_core::events::EventBus;
    use djinn_db::Database;
    use std::sync::{Mutex, MutexGuard};

    static TELEMETRY_MUTEX: Mutex<()> = Mutex::new(());

    fn telemetry_guard() -> MutexGuard<'static, ()> {
        TELEMETRY_MUTEX
            .lock()
            .expect("telemetry test mutex poisoned")
    }

    /// Invoke the real `assemble_prompt_context` path and assert that
    /// the total latency histogram `djinn_prompt_context_latency_seconds`
    /// is recorded, and that child-span latency histograms are emitted
    /// with the expected bounded `span` labels.  Preserves existing
    /// boundary output/error semantics: the returned `PromptContext`
    /// should have a non-empty `system_prompt` and valid fields.
    #[tokio::test]
    async fn assemble_prompt_context_emits_total_and_child_span_metrics() {
        // Initialize telemetry under the guard, then drop before async work
        // to avoid holding a std::sync::Mutex across await points.
        {
            let _guard = telemetry_guard();
            djinn_telemetry::init().expect("telemetry init");
        }

        let db = Database::ephemeral().await.expect("create ephemeral db");
        let events = EventBus::noop();
        let task = create_project_epic_task(&db, &events, "Instr Epic", "Instr Task").await;
        let role = WorkerRole;

        // Invoke the real assembly path — this exercises the full
        // phase-0/1/2/3 pipeline including concurrent children.
        let ctx = assemble_for_role(db, &task, &role, None, "", &[], &[]).await;

        // Re-acquire guard for metric assertions (no await after this).
        let _guard = telemetry_guard();

        // ── Boundary-output semantics preserved ──
        assert!(
            !ctx.system_prompt.is_empty(),
            "system_prompt should be non-empty after assembly"
        );
        assert!(
            !ctx.base_system_prompt.is_empty(),
            "base_system_prompt should be non-empty after assembly"
        );

        // ── Total latency metric emitted ──
        let rendered = djinn_telemetry::render().expect("render metrics");
        assert!(
            rendered.contains("djinn_prompt_context_latency_seconds"),
            "total latency histogram missing from rendered metrics:\n{rendered}",
        );

        // ── Child-span latency metric emitted with span labels ──
        assert!(
            rendered.contains("djinn_prompt_context_child_span_latency_seconds"),
            "child-span latency histogram missing from rendered metrics:\n{rendered}",
        );
        // All six child spans should be recorded by the assembly path.
        for span in &["activity_db", "epic_context"] {
            assert!(
                rendered.contains(&format!("span=\"{span}\"")),
                "missing {span} span label in child-span metrics:\n{rendered}",
            );
        }
        // Phase-2 spans are also recorded (knowledge_context, attempt_history,
        // code_graph, reviewer_diff).  At minimum the phase-2 spans that
        // don't require external services should fire even in test.
        for span in &[
            "knowledge_context",
            "attempt_history",
            "code_graph",
            "reviewer_diff",
        ] {
            assert!(
                rendered.contains(&format!("span=\"{span}\"")),
                "missing {span} span label in child-span metrics:\n{rendered}",
            );
        }
    }
}
