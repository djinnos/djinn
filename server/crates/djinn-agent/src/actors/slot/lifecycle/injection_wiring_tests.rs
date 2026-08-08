//! Production-shaped guards for knowledge injection's *retrieval wiring*.
//!
//! Proposal `5205` replaced boolean scope-overlap eligibility with a six-signal
//! RRF fusion, and every signal was proven in isolation. Injection still
//! collapsed on deploy — average candidates per retrieval fell from 50.0 to 0.2
//! and 27 of 35 traces recorded `outcome='empty'` — because the *call site* fed
//! the base-tree loader a value that is not a filesystem path, so validated
//! scope was empty for 100% of production dispatches and the fusion ran on its
//! lexical list alone.
//!
//! Every test here therefore drives the same entry point dispatch drives
//! (`assemble_prompt_context`) with the same `project_path` dispatch passes
//! (`task.project_id`), and asserts the side effect that matters: the scoped
//! note reaches the rendered prompt. A test that supplies its own
//! `ListedBaseTree` cannot fail this way and so cannot guard against it.

use super::*;

use djinn_core::events::EventBus;
use djinn_db::NoteRepository;
use tokio_util::sync::CancellationToken;

use super::test_support::create_project_epic_task;
use crate::roles::WorkerRole;
use crate::test_helpers::agent_context_from_db;

/// A repository path the task text will name, and the note scope that covers it.
const TASK_FILE_PATH: &str = "server/crates/djinn-agent/src/actors/slot/reply_loop.rs";
const NOTE_SCOPE_DIR: &str = "server/crates/djinn-agent/src/actors/slot";

/// Note prose sharing **no** term with [`task_text`], so nothing here can be
/// retrieved lexically and the assertion can only be satisfied by scope.
const DISJOINT_NOTE_BODY: &str = "Marmalade cartography enumerates purple velvet. Zebra hibernation quietly \
     precedes frozen tundra migration across nine basalt plateaus.";

/// Task title and description whose vocabulary is disjoint from
/// [`DISJOINT_NOTE_BODY`] apart from the repository path itself — which is not
/// part of `notes.search_vector` (that column covers title, tags, and content).
fn task_text() -> (String, String) {
    (
        "Rework carousel throttling".to_owned(),
        format!("Touches {TASK_FILE_PATH} for the carousel throttling rework."),
    )
}

/// Stand up a real Git repository at `{root}/{project_id}.git` holding
/// `tracked`, committed on `branch`.
///
/// This is deliberately a real repository driven through `git`, not a synthetic
/// [`ListedBaseTree`]: the defect under test was that production never reached
/// `git` at all, so a fake tree provider would paper straight over it.
async fn seed_project_mirror(
    root: &std::path::Path,
    project_id: &str,
    branch: &str,
    tracked: &[&str],
) {
    let repo = root.join(format!("{project_id}.git"));
    std::fs::create_dir_all(&repo).expect("create mirror dir");
    let run = |args: Vec<String>| {
        let repo = repo.clone();
        async move {
            djinn_git::run_git_command(repo, args)
                .await
                .unwrap_or_else(|error| panic!("git failed: {error}"));
        }
    };
    let owned = |args: &[&str]| {
        args.iter()
            .map(|a| (*a).to_owned())
            .collect::<Vec<String>>()
    };

    run(owned(&["init", "-q", "-b", branch])).await;
    run(owned(&["config", "user.email", "guard@example.test"])).await;
    run(owned(&["config", "user.name", "Injection Guard"])).await;
    for path in tracked {
        let file = repo.join(path);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).expect("create tracked parent dir");
        }
        std::fs::write(&file, b"// tracked\n").expect("write tracked file");
    }
    run(owned(&["add", "-A"])).await;
    run(owned(&["commit", "-q", "-m", "seed base revision"])).await;
}

/// Create one knowledge note with the given scope and a body that cannot be
/// found lexically from the task text.
async fn seed_disjoint_note(
    db: &djinn_db::Database,
    project_id: &str,
    title: &str,
    scope_paths: &str,
) -> djinn_memory::Note {
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .create_with_scope(
            project_id,
            title,
            DISJOINT_NOTE_BODY,
            "pattern",
            None,
            "[]",
            scope_paths,
        )
        .await
        .expect("create scoped note");
    note_repo
        .set_confidence(&note.id, 0.9)
        .await
        .expect("set confidence");
    note
}

/// Drive prompt assembly exactly the way `supervisor_impl::stage` does,
/// including its `project_path` value.
async fn assemble_like_dispatch(db: djinn_db::Database, task: &Task) -> PromptContext {
    let app_state = agent_context_from_db(db, CancellationToken::new());
    let worktree = crate::test_helpers::test_tempdir("injection-wiring-worktree-");
    let role = WorkerRole;
    assemble_prompt_context(PromptContextInputs {
        task,
        runtime_role: &role,
        role_for_epic_check: &role,
        // `stage.rs` sets this to `task.project_id`, because the prompt hands it
        // to MCP tools as `project=…`. Anything that treats it as a path is a bug.
        project_path: &task.project_id,
        worktree_path: worktree.path(),
        conflict_ctx: None,
        merge_validation_ctx: None,
        prompt_setup_commands: None,
        system_prompt_extensions: "",
        resolved_skills: &[],
        app_state: &app_state,
        read_sources: &[],
        worker_resume_note: None,
        arbiter_directive: None,
        ci_adjudication_bundle: None,
        mcp_server_instructions: &std::collections::BTreeMap::new(),
        extension_diagnostics: &[],
        cancellation: None,
        memory_intent_planner: None,
    })
    .await
}

/// The pin on the wiring defect itself, with no database in the way.
///
/// `project_path` is `task.project_id`, so treating it as a repository root
/// resolves nothing. The mirror must be what base-tree resolution finds.
#[test]
fn base_tree_root_resolves_the_project_mirror_not_the_project_id() {
    let mut env = knowledge_context_test_env_guard();
    let root = crate::test_helpers::test_tempdir("injection-wiring-mirror-");
    env.set_mirror_root(root.path());
    let project_id = "019ea3bd-a305-73e3-806c-4edcc96ebfe2";

    assert_eq!(
        resolve_base_tree_root(project_id, project_id),
        None,
        "with no mirror on disk there is no base-tree root: the project id is \
         not a path, and inventing one would validate prose against the wrong tree"
    );

    let mirror = root.path().join(format!("{project_id}.git"));
    std::fs::create_dir_all(&mirror).expect("create mirror dir");
    assert_eq!(
        resolve_base_tree_root(project_id, project_id),
        Some(mirror),
        "the project's bare mirror is the base-tree root dispatch must read"
    );
}

/// AC-shaped guard: a corpus where most notes carry scope paths and the task
/// text shares **no** lexical term with any of them still yields injected
/// knowledge, through the real dispatch wiring.
///
/// Against the pre-fix wiring this fails: `load_base_tree` is handed the project
/// id, finds no repository, and `derive_task_scope_paths` returns an empty scope
/// with `tree_provider_unavailable`, so the scope signal contributes nothing and
/// the deliberately disjoint corpus is unreachable.
#[tokio::test]
async fn scoped_notes_reach_the_prompt_when_the_task_text_shares_no_lexical_term() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    let mirror_root = crate::test_helpers::test_tempdir("injection-wiring-mirror-");
    env.set_mirror_root(mirror_root.path());

    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let mut task = create_project_epic_task(&db, &events, "Wiring epic", "Wiring task").await;
    let (title, description) = task_text();
    task.title = title;
    task.description = description;
    task.design = String::new();

    seed_project_mirror(
        mirror_root.path(),
        &task.project_id,
        &project_target_branch(
            &task,
            &agent_context_from_db(db.clone(), CancellationToken::new()),
        )
        .await,
        &[TASK_FILE_PATH, "README.md"],
    )
    .await;

    // Production shape: 8 of 10 eligible notes carry scope paths (measured at
    // 8,498 of 10,482 active eligible notes, 81%). None of them is lexically
    // reachable from the task text.
    let mut scoped = Vec::new();
    for index in 0..8 {
        scoped.push(
            seed_disjoint_note(
                &db,
                &task.project_id,
                &format!("Marmalade cartography note {index}"),
                &format!("[\"{NOTE_SCOPE_DIR}\"]"),
            )
            .await,
        );
    }
    for index in 0..2 {
        seed_disjoint_note(
            &db,
            &task.project_id,
            &format!("Unscoped marmalade note {index}"),
            "[]",
        )
        .await;
    }

    let ctx = assemble_like_dispatch(db, &task).await;

    let injected: Vec<&str> = scoped
        .iter()
        .map(|note| note.permalink.as_str())
        .filter(|permalink| ctx.system_prompt.contains(permalink))
        .collect();
    assert!(
        !injected.is_empty(),
        "no scoped note reached the prompt: the validated-scope signal \
         contributed nothing even though 8 of 10 eligible notes are scoped to \
         {NOTE_SCOPE_DIR} and the task names {TASK_FILE_PATH}"
    );
}

/// The lexical list is one contributing signal, not the eligibility gate.
///
/// A note sharing *some* of the task's vocabulary must still be retrievable.
/// The pre-fix `Ranked` mode AND-joined the first twelve query terms, so a whole
/// title plus description had to appear in one note — measured against the
/// production corpus, 22 of 25 recent tasks matched zero notes that way, and
/// with scope dead that empty list was the entire candidate universe.
#[tokio::test]
async fn partial_lexical_overlap_still_produces_candidates() {
    let mut env = knowledge_context_test_env_guard();
    env.clear();
    // No mirror: this test isolates the lexical signal, so validated scope must
    // stay empty and cannot be what satisfies the assertion.
    let empty_root = crate::test_helpers::test_tempdir("injection-wiring-no-mirror-");
    env.set_mirror_root(empty_root.path());

    let db = djinn_db::Database::ephemeral().await.expect("ephemeral db");
    let events = EventBus::noop();
    let mut task = create_project_epic_task(&db, &events, "Lexical epic", "Lexical task").await;
    task.title = "Rework carousel throttling".to_owned();
    task.description = "The carousel stalls whenever a deferred repaint arrives \
         before the sampler window closes and the queue drains."
        .to_owned();
    task.design = String::new();

    // Shares "carousel" with the task and nothing else. Under an AND-joined
    // twelve-term query this note is unreachable.
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let overlapping = note_repo
        .create(
            &task.project_id,
            "Carousel behaviour",
            "Marmalade cartography of the carousel, unrelated to everything else.",
            "pattern",
            "[]",
        )
        .await
        .expect("create overlapping note");
    note_repo
        .set_confidence(&overlapping.id, 0.9)
        .await
        .expect("set confidence");

    let ctx = assemble_like_dispatch(db, &task).await;
    assert!(
        ctx.system_prompt.contains(&overlapping.permalink),
        "a note overlapping one query term was not retrieved: the lexical signal \
         is still gating on every term rather than ranking by overlap"
    );
}
