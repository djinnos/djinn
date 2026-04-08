use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::commands::run_commands;
use crate::context::AgentContext;
use crate::message::{Conversation, Message};
use crate::prompts::TaskContext;
use crate::provider::LlmProvider;
use crate::provider::create_provider;
use crate::roles::AgentRole;
use crate::verification::settings::load_settings;
use djinn_core::models::SessionStatus;
use djinn_core::models::TransitionAction;
use djinn_db::SessionRepository;
use djinn_db::TaskRepository;
use djinn_db::repositories::session::CreateSessionParams;

use super::reply_loop::error_handling::is_orphaned_tool_call_error_str;
use super::reply_loop::{ReplyLoopContext, run_reply_loop};
use super::*;
use crate::AgentType;
use crate::task_merge::interrupt_paused_worker_session;

fn is_database_locked(error: &djinn_db::Error) -> bool {
    match error {
        djinn_db::Error::Sqlx(sqlx_err) => sqlx_err
            .as_database_error()
            .and_then(|db_err| db_err.code())
            .map(|code| matches!(code.as_ref(), "5" | "6" | "517"))
            .unwrap_or_else(|| {
                let msg = sqlx_err.to_string().to_ascii_lowercase();
                msg.contains("database is locked") || msg.contains("database table is locked")
            }),
        other => {
            let msg = other.to_string().to_ascii_lowercase();
            msg.contains("database is locked") || msg.contains("database table is locked")
        }
    }
}

async fn retry_task_transition_on_locked<F, Fut, T>(mut op: F) -> Result<T, djinn_db::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, djinn_db::Error>>,
{
    const MAX_RETRIES: u32 = 5;
    const BASE_DELAY_MS: u64 = 200;

    let mut attempt = 0;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) if is_database_locked(&err) && attempt < MAX_RETRIES => {
                attempt += 1;
                let delay = BASE_DELAY_MS * 2u64.pow(attempt - 1);
                tracing::debug!(
                    attempt,
                    delay_ms = delay,
                    "Lifecycle: database locked during task transition, retrying after backoff"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Sort tool schemas deterministically for prompt-cache stability (ADR-048
/// §2C).  Built-in tools (indices `0..builtin_count`) are sorted
/// alphabetically among themselves, then MCP tools (`builtin_count..`) are
/// sorted alphabetically among themselves, keeping the two groups in order.
fn sort_tool_schemas(tools: &mut [serde_json::Value], builtin_count: usize) {
    let key = |v: &serde_json::Value| -> String {
        v.get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string()
    };
    let split = builtin_count.min(tools.len());
    tools[..split].sort_by_key(|a| key(a));
    tools[split..].sort_by_key(|a| key(a));
}

/// Build a git commit message from an optional title and body.
/// Truncates the title to 72 characters. Returns `None` if both are empty.
fn format_commit_message(title: Option<&str>, body: Option<&str>) -> Option<String> {
    let title = title.map(|t| {
        let t = t.trim();
        if t.len() > 72 {
            format!("{}...", &t[..69])
        } else {
            t.to_string()
        }
    });
    let body = body.and_then(|b| {
        let b = b.trim();
        if b.is_empty() { None } else { Some(b) }
    });
    match (title.as_deref(), body) {
        (Some(t), Some(b)) if !t.is_empty() => Some(format!("{t}\n\n{b}")),
        (Some(t), None) if !t.is_empty() => Some(t.to_string()),
        (_, Some(b)) => Some(b.to_string()),
        _ => None,
    }
}

/// Standalone async function that runs the full per-task lifecycle:
/// load -> worktree -> session -> reply loop -> post-session work -> cleanup.
/// Verification runs as a separate background task after the slot is freed.
///
/// Compaction is handled as an inline loop (no supervisor messages). The reply
/// loop returns its result directly instead of sending SessionCompleted back to
/// an actor.
///
/// Sends `SlotEvent::Free` on normal completion and `SlotEvent::Killed` when
/// cancelled via `cancel`.
pub(crate) struct TaskLifecycleParams {
    pub task_id: String,
    pub project_path: String,
    pub model_id: String,
    pub role: Arc<dyn AgentRole>,
    pub app_state: AgentContext,
    pub cancel: CancellationToken,
    pub pause: CancellationToken,
    pub event_tx: mpsc::Sender<SlotEvent>,
    /// Additional system prompt text from the DB role's system_prompt_extensions.
    /// Appended after the base rendered prompt. Empty string = no extension.
    pub system_prompt_extensions: String,
    /// Auto-improvement amendments from the DB role's learned_prompt.
    /// Appended after system_prompt_extensions. None = not set.
    pub learned_prompt: Option<String>,
    /// MCP server names from the DB role config (JSON → Vec<String>).
    /// Passed through for future wiring by task `norv`.
    pub mcp_servers: Vec<String>,
    /// Skill names from the DB role config (JSON → Vec<String>).
    /// Passed through for future wiring by task `9trm`.
    pub skills: Vec<String>,
    /// Override for the verification command from the DB role.
    /// When Some, used instead of the project's .djinn/settings.json verification.
    pub role_verification_command: Option<String>,
    #[cfg(test)]
    pub mcp_registry_override: Option<crate::mcp_client::McpToolRegistry>,
    /// Test-only: inject a pre-built provider, bypassing credential loading.
    /// When `Some`, `parse_model_id` and `load_provider_credential` are skipped.
    #[cfg(test)]
    pub provider_override: Option<Arc<dyn LlmProvider>>,
}

pub(crate) async fn run_task_lifecycle(params: TaskLifecycleParams) -> anyhow::Result<()> {
    let TaskLifecycleParams {
        task_id,
        project_path,
        mut model_id,
        role,
        mut app_state,
        cancel,
        pause,
        event_tx,
        mut system_prompt_extensions,
        mut learned_prompt,
        mut mcp_servers,
        mut skills,
        mut role_verification_command,
        #[cfg(test)]
        mcp_registry_override,
        #[cfg(test)]
        provider_override,
    } = params;
    let emit_step = |task_id: &str, step: &str, detail: serde_json::Value| {
        app_state
            .event_bus
            .send(djinn_core::events::DjinnEventEnvelope::task_lifecycle_step(
                task_id, step, &detail,
            ));
    };

    // Helper macros for early-exit slot events. These send to a dummy channel
    // (slot_id 0 is never a real slot). The authoritative SlotEvent::Free /
    // SlotEvent::Killed is emitted by SlotActor::emit_completion_event after
    // the lifecycle future resolves.
    macro_rules! return_free {
        () => {{
            let _ = event_tx
                .send(SlotEvent::Free {
                    slot_id: 0,
                    model_id: model_id.clone(),
                    task_id: task_id.clone(),
                })
                .await;
            return Ok(());
        }};
    }
    macro_rules! return_killed {
        () => {{
            let _ = event_tx
                .send(SlotEvent::Killed {
                    slot_id: 0,
                    model_id: model_id.clone(),
                    task_id: task_id.clone(),
                })
                .await;
            return Ok(());
        }};
    }

    if cancel.is_cancelled() {
        return_killed!();
    }
    if pause.is_cancelled() {
        return_free!();
    }

    // ── Load task ──────────────────────────────────────────────────────────────
    let task = match load_task(&task_id, &app_state).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to load task");
            return_free!();
        }
    };
    let conflict_ctx = conflict_context_for_dispatch(&task.id, &app_state).await;
    let merge_validation_ctx = merge_validation_context_for_dispatch(&task.id, &app_state).await;

    // ── Specialist override: if the task has an explicit agent_type, load that role ──
    // The actor already loaded the default DB role for system_prompt_extensions / skills.
    // If the task carries a specialist name, reload role fields from that specialist,
    // including the effective runtime role used for prompt identity and transitions.
    let mut runtime_role = role.clone();
    if let Some(ref specialist_name) = task.agent_type
        && !specialist_name.is_empty()
    {
        let role_repo =
            djinn_db::AgentRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        let specialist = role_repo
            .get_by_name_for_project(&task.project_id, specialist_name)
            .await
            .unwrap_or(None);
        if let Some(ref r) = specialist {
            tracing::debug!(
                task_id = %task.short_id,
                specialist = %r.name,
                base_role = %r.base_role,
                "Lifecycle: overriding role config from specialist agent_type"
            );
            if let Ok(agent_type) = AgentType::from_str(&r.base_role) {
                runtime_role = crate::roles::role_impl_for(agent_type);
            } else {
                tracing::warn!(
                    task_id = %task.short_id,
                    specialist = %r.name,
                    base_role = %r.base_role,
                    "Lifecycle: specialist base_role is unknown; keeping injected role"
                );
            }
            system_prompt_extensions = r.system_prompt_extensions.clone();
            learned_prompt = r.learned_prompt.clone();
            mcp_servers = djinn_core::models::parse_json_array(&r.mcp_servers);
            skills = djinn_core::models::parse_json_array(&r.skills);
            role_verification_command = r.verification_command.clone();
            if let Some(ref preferred) = r.model_preference
                && !preferred.is_empty()
            {
                model_id = preferred.clone();
            }
        }
    }

    tracing::info!(
        task_id = %task.short_id,
        task_uuid = %task.id,
        project_id = %task.project_id,
        model_id = %model_id,
        role = %runtime_role.config().name,
        task_status = %task.status,
        has_conflict_context = conflict_ctx.is_some(),
        has_merge_validation_context = merge_validation_ctx.is_some(),
        "Lifecycle: dispatch accepted; preparing session"
    );

    // ── Transition task to in-progress ────────────────────────────────────────
    if let Err(e) = transition_start(&task, runtime_role.config().start_action, &app_state).await {
        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: transition_start failed");
        return_free!();
    }

    // Notify the frontend immediately so it can show the agent avatar while
    // worktree/setup is still running.
    app_state
        .event_bus
        .send(djinn_core::events::DjinnEventEnvelope::session_dispatched(
            &task.project_id,
            &task.id,
            &model_id,
            runtime_role.config().name,
        ));
    tracing::info!(
        task_id = %task_id,
        "Lifecycle: emitted session.dispatched SSE event"
    );

    // ── Parse model ID and load credentials ───────────────────────────────────
    // In tests, a provider_override bypasses credential loading entirely.
    #[cfg(test)]
    let _credential_skipped = provider_override.is_some();
    #[cfg(not(test))]
    let _credential_skipped = false;

    let (catalog_provider_id, model_name, provider_credential) = if _credential_skipped {
        // Test seam: skip credential/catalog lookups.
        (String::new(), String::new(), None)
    } else {
        let (cpid, mname) = match parse_model_id(&model_id) {
            Ok((provider_id, name)) => {
                // Settings may store display names (e.g. "GPT-5.3 Codex") or
                // bare suffixes (e.g. "GLM-4.7" for internal "hf:zai-org/GLM-4.7").
                // Resolve to the actual model ID for the provider API.
                let resolved = app_state
                    .catalog
                    .list_models(&provider_id)
                    .iter()
                    .find(|m| {
                        let bare = m.id.rsplit('/').next().unwrap_or(&m.id);
                        m.id == name || m.name == name || bare == name
                    })
                    .map(|m| m.id.clone())
                    .unwrap_or(name);
                (provider_id, resolved)
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: invalid model ID");
                transition_interrupted(
                    &task_id,
                    runtime_role.config().release_action,
                    &e.to_string(),
                    &app_state,
                )
                .await;
                return_free!();
            }
        };
        emit_step(
            &task.id,
            "credential_loading",
            serde_json::json!({"provider_id": cpid}),
        );
        let cred = match load_provider_credential(&cpid, &app_state).await {
            Ok(cred) => cred,
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: missing credential");
                transition_interrupted(
                    &task_id,
                    runtime_role.config().release_action,
                    &e.to_string(),
                    &app_state,
                )
                .await;
                return_free!();
            }
        };
        (cpid, mname, Some(cred))
    };

    // ── Prepare worktree / paused-session resume context ──────────────────────
    let project_dir = PathBuf::from(&project_path);

    let paused = find_paused_session_record(&task_id, runtime_role.config().name, &app_state).await;

    // `resume_record_id` is set when we can resume a paused worker session
    // (same model, same agent type, worktree intact, conversation file present).
    let mut resume_record_id: Option<String> = None;

    emit_step(&task.id, "worktree_creating", serde_json::json!({}));
    emit_step(&task.id, "branch_creating", serde_json::json!({}));
    let mut worktree_conflict_files: Option<Vec<String>> = None;
    let worktree_path = if let Some(paused) = paused {
        if let Some(paused_worktree_path) = paused.worktree_path.as_deref().map(PathBuf::from) {
            if paused.model_id != model_id {
                tracing::info!(
                    task_id = %task_id,
                    paused_model_id = %paused.model_id,
                    requested_model_id = %model_id,
                    "Lifecycle: paused session model mismatch; starting fresh session"
                );
                match prepare_worktree(&project_dir, &task, &app_state).await {
                    Ok((p, cf)) => {
                        worktree_conflict_files = cf;
                        p
                    }
                    Err(e) => {
                        tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                        return_free!();
                    }
                }
            } else if paused.agent_type != runtime_role.config().name {
                tracing::info!(
                    task_id = %task_id,
                    paused_agent_type = %paused.agent_type,
                    needed_agent_type = %runtime_role.config().name,
                    "Lifecycle: paused session agent type mismatch; starting fresh session"
                );
                match prepare_worktree(&project_dir, &task, &app_state).await {
                    Ok((p, cf)) => {
                        worktree_conflict_files = cf;
                        p
                    }
                    Err(e) => {
                        tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                        return_free!();
                    }
                }
            } else if !paused_worktree_path.exists() || !paused_worktree_path.is_dir() {
                let session_repo =
                    SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                let _ = session_repo
                    .update(
                        &paused.id,
                        SessionStatus::Interrupted,
                        paused.tokens_in,
                        paused.tokens_out,
                    )
                    .await;
                tracing::warn!(
                    task_id = %task_id,
                    session_record_id = %paused.id,
                    worktree = %paused_worktree_path.display(),
                    "Lifecycle: paused session worktree missing; finalized as interrupted"
                );
                match prepare_worktree(&project_dir, &task, &app_state).await {
                    Ok((p, cf)) => {
                        worktree_conflict_files = cf;
                        p
                    }
                    Err(e) => {
                        tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                        return_free!();
                    }
                }
            } else {
                // Model match, worktree intact — resume the paused session
                // instead of starting fresh (agent_type already filtered by query).
                tracing::info!(
                    task_id = %task_id,
                    session_record_id = %paused.id,
                    "Lifecycle: resuming paused session; reusing worktree"
                );
                resume_record_id = Some(paused.id);
                paused_worktree_path
            }
        } else {
            tracing::warn!(task_id = %task_id, session_record_id = %paused.id, "Lifecycle: paused session missing worktree; starting fresh session");
            match prepare_worktree(&project_dir, &task, &app_state).await {
                Ok((p, cf)) => {
                    worktree_conflict_files = cf;
                    p
                }
                Err(e) => {
                    tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                    return_free!();
                }
            }
        }
    } else {
        match prepare_worktree(&project_dir, &task, &app_state).await {
            Ok((p, cf)) => {
                worktree_conflict_files = cf;
                p
            }
            Err(e) => {
                // Do NOT call transition_interrupted here — that would release
                // the task back to "open" immediately, and return_free!() would
                // trigger redispatch, creating a tight infinite loop when
                // prepare_worktree keeps failing (e.g. concurrent git ops racing).
                // Instead, leave the task in "in_progress" so the coordinator's
                // 30-second stuck-task detector releases it with natural backoff.
                tracing::error!(task_id = %task_id, error = %e, "Lifecycle: prepare_worktree failed; leaving task in_progress for stuck-detector recovery");
                return_free!();
            }
        }
    };

    // ── Persist worktree rebase conflict metadata if detected ────────────────
    if let Some(ref conflict_files) = worktree_conflict_files {
        let target_branch = default_target_branch(&task.project_id, &app_state).await;
        let meta = serde_json::json!({
            "conflicting_files": conflict_files,
            "base_branch": format!("task/{}", task.short_id),
            "merge_target": target_branch,
        });
        let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        if let Err(e) = task_repo
            .set_merge_conflict_metadata(&task.id, Some(&meta.to_string()))
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to persist merge conflict metadata");
        }
    }

    // ── Role-specific worktree preparation (e.g. conflict resolver merge) ────
    emit_step(
        &task.id,
        "worktree_created",
        serde_json::json!({"path": worktree_path.display().to_string()}),
    );
    emit_step(&task.id, "branch_created", serde_json::json!({}));

    let _ = runtime_role
        .prepare_worktree(&worktree_path, &task, &app_state)
        .await;

    // ── ADR-050 Chunk C: canonical-graph cache warming ───────────────────────
    // ONLY the architect triggers a warm.  Originally every role warmed so
    // that workers/reviewers/planners/lead would receive a freshly rendered
    // `repo_map` note through the standard note pipeline — but that meant
    // every non-architect dispatch either hit the cache (fine) or serialized
    // on the server-wide `indexer_lock` behind a full SCIP rebuild, which on
    // cold cache took tens of minutes and wedged the dispatcher for every
    // other session.  Workers tolerate a stale skeleton: whatever `repo_map`
    // note the most recent architect warm left in the DB is picked up by
    // the normal note-loading machinery.  Only the architect actually
    // *needs* the graph fresh (for `code_graph` against `origin/main`), and
    // even there we bound the wait so a slow build can never wedge dispatch.
    //
    // Best-effort: `ensure_canonical_graph` may legitimately fail (network
    // blip on `git fetch`, missing rust-analyzer, compile error on cold
    // cache) or exceed the bounded wait.  We log and let the agent runtime
    // start anyway — the architect degrades to "no fresh skeleton" instead
    // of refusing to run.
    if runtime_role.config().name == "architect"
        && let Some(warmer) = app_state.canonical_graph_warmer.clone()
    {
        const WARM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
        let warm_started = std::time::Instant::now();
        match tokio::time::timeout(WARM_TIMEOUT, warmer.warm(&task.project_id, &project_dir)).await
        {
            Ok(Ok(())) => {
                tracing::debug!(
                    task_id = %task_id,
                    project_id = %task.project_id,
                    elapsed_ms = warm_started.elapsed().as_millis() as u64,
                    "Lifecycle: canonical graph cache warmed"
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    task_id = %task_id,
                    project_id = %task.project_id,
                    error = %e,
                    elapsed_ms = warm_started.elapsed().as_millis() as u64,
                    "Lifecycle: ensure_canonical_graph warming failed; architect will run without a fresh skeleton"
                );
            }
            Err(_) => {
                tracing::warn!(
                    task_id = %task_id,
                    project_id = %task.project_id,
                    timeout_secs = WARM_TIMEOUT.as_secs(),
                    "Lifecycle: ensure_canonical_graph warming exceeded timeout; architect proceeds without fresh skeleton (background warm may still complete)"
                );
            }
        }
    }

    // ── ADR-050 Chunk C: architect working_root pin ──────────────────────────
    // Architect sessions additionally read against the canonical `_index/`
    // worktree pinned to `origin/main`, NOT the per-task review-task
    // worktree.  We:
    //   1. Best-effort create `<project_root>/.djinn/worktrees/_index` if it
    //      does not yet exist (so `read`/`shell` succeed against it).  The
    //      canonical graph is already warm by this point thanks to the
    //      warming block above, so the architect's first `code_graph` call
    //      hits the cache.
    //   2. Bake the index-tree path into `app_state.working_root` so the
    //      tool dispatch layer routes `read`/`shell`/`lsp`/`code_graph`
    //      against it.  Worker/reviewer/planner/lead leave this `None` so
    //      their tools continue to resolve against the per-task worktree.
    if runtime_role.config().name == "architect" {
        let index_tree_path = djinn_core::index_tree::index_tree_path(&project_dir);
        if !index_tree_path.join(".git").exists()
            && let Ok(git) = app_state.git_actor(&project_dir).await
        {
            let _ = git
                .run_command(vec!["worktree".into(), "prune".into()])
                .await;
            let attempt = git
                .run_command(vec![
                    "worktree".into(),
                    "add".into(),
                    "--detach".into(),
                    index_tree_path.to_string_lossy().into_owned(),
                    "origin/main".into(),
                ])
                .await;
            if attempt.is_err() {
                let _ = git
                    .run_command(vec![
                        "worktree".into(),
                        "add".into(),
                        "--detach".into(),
                        index_tree_path.to_string_lossy().into_owned(),
                        "HEAD".into(),
                    ])
                    .await;
            }
        }
        if index_tree_path.exists() {
            tracing::info!(
                task_id = %task_id,
                index_tree = %index_tree_path.display(),
                "Lifecycle: pinning architect working_root to canonical index tree"
            );
            app_state.working_root = Some(index_tree_path);
        } else {
            tracing::warn!(
                task_id = %task_id,
                "Lifecycle: index tree unavailable; architect will read against review worktree"
            );
        }
    }

    emit_step(&task.id, "preflight_checking", serde_json::json!({}));
    if !worktree_path.exists() || !worktree_path.is_dir() {
        let diag = runtime_fs_diagnostics(&project_path, &worktree_path);
        tracing::warn!(task_id = %task_id, diag = %diag, "Lifecycle: worktree preflight failed");
        transition_interrupted(
            &task_id,
            runtime_role.config().release_action,
            "worktree preflight failed",
            &app_state,
        )
        .await;
        return_free!();
    }
    // Verify the worktree is a valid git working tree, not just an empty
    // directory left behind by a partially failed `git worktree add`.
    // Worktrees have a `.git` *file* (not directory) that points back to
    // the main repository.  Also verify the branch ref is intact via
    // `git rev-parse HEAD` inside the worktree.
    let git_link = worktree_path.join(".git");
    let git_link_valid = git_link.exists() && (git_link.is_file() || git_link.is_dir());
    let rev_parse_ok = if git_link_valid {
        tokio::process::Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(&worktree_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    } else {
        false
    };
    if !git_link_valid || !rev_parse_ok {
        tracing::warn!(
            task_id = %task_id,
            worktree = %worktree_path.display(),
            git_link_exists = git_link.exists(),
            rev_parse_ok = rev_parse_ok,
            "Lifecycle: worktree preflight failed — directory exists but git state is broken"
        );
        transition_interrupted(
            &task_id,
            runtime_role.config().release_action,
            "worktree preflight failed: broken git state",
            &app_state,
        )
        .await;
        return_free!();
    }
    emit_step(&task.id, "preflight_passed", serde_json::json!({}));

    // ── Resolve role-level MCP servers ────────────────────────────────────────
    // Load the project MCP server registry from .djinn/settings.json and resolve
    // each server name in the role's mcp_servers list.  Unknown names are logged
    // as warnings and skipped — they never block the session from starting.
    //
    // Default roles have empty mcp_servers, so this block is a no-op for them.
    let resolved_mcp_servers = if !mcp_servers.is_empty() {
        let registry = crate::verification::settings::load_mcp_server_registry(&worktree_path);
        let resolved = crate::verification::settings::resolve_mcp_servers(
            &task.short_id,
            runtime_role.config().name,
            &mcp_servers,
            &registry,
        );
        tracing::info!(
            task_id = %task.short_id,
            role = %runtime_role.config().name,
            requested_count = mcp_servers.len(),
            resolved_count = resolved.len(),
            "Lifecycle: resolved role MCP servers"
        );
        resolved
            .into_iter()
            .map(|(name, cfg)| (name, cfg.clone()))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    // Connect to resolved MCP servers and discover their tool definitions.
    // Unreachable or misconfigured servers are logged and skipped (non-fatal).
    let mcp_registry = {
        #[cfg(test)]
        {
            if let Some(registry) = mcp_registry_override {
                Some(registry)
            } else if !resolved_mcp_servers.is_empty() {
                crate::mcp_client::connect_and_discover(
                    &task.short_id,
                    runtime_role.config().name,
                    &resolved_mcp_servers,
                )
                .await
            } else {
                None
            }
        }
        #[cfg(not(test))]
        {
            if !resolved_mcp_servers.is_empty() {
                crate::mcp_client::connect_and_discover(
                    &task.short_id,
                    runtime_role.config().name,
                    &resolved_mcp_servers,
                )
                .await
            } else {
                None
            }
        }
    };

    // ── Load and resolve skills from worktree .djinn/skills/ ─────────────────
    // Skills are markdown files with YAML frontmatter. Missing skills are logged
    // as warnings and skipped — they never block the session from starting.
    let resolved_skills = if !skills.is_empty() {
        let loaded = crate::skills::load_skills(&worktree_path, &skills);
        tracing::info!(
            task_id = %task.short_id,
            role = %runtime_role.config().name,
            requested_count = skills.len(),
            resolved_count = loaded.len(),
            "Lifecycle: resolved role skills"
        );
        loaded
    } else {
        Vec::new()
    };

    let (prompt_setup_commands, prompt_verification_commands, prompt_verification_rules) = {
        let settings = load_settings(&worktree_path).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to load project settings, using defaults");
            Default::default()
        });
        let setup_specs = settings.setup;
        let verification_rules = settings.verification_rules;
        let prompt_setup_commands = format_command_details(&setup_specs);
        // Role-level verification_command overrides .djinn/settings.json when set.
        let prompt_verification_commands = if let Some(ref cmd) = role_verification_command {
            if !cmd.trim().is_empty() {
                tracing::debug!(
                    task_id = %task.short_id,
                    command = %cmd,
                    "Lifecycle: using role-level verification_command override"
                );
                Some(cmd.clone())
            } else {
                None
            }
        } else {
            None
        };
        if !setup_specs.is_empty() {
            let setup_start = std::time::Instant::now();
            tracing::info!(
                task_id = %task.short_id,
                command_count = setup_specs.len(),
                "Lifecycle: running setup commands"
            );
            let mut setup_results = Vec::new();
            let mut setup_error: Option<anyhow::Error> = None;
            for spec in &setup_specs {
                emit_step(
                    &task.id,
                    "setup_command_started",
                    serde_json::json!({"name": spec.name, "command": spec.command}),
                );
                match run_commands(std::slice::from_ref(spec), &worktree_path).await {
                    Ok(mut results) => {
                        if let Some(result) = results.pop() {
                            let status = if result.exit_code == 0 { "ok" } else { "error" };
                            emit_step(
                                &task.id,
                                "setup_command_finished",
                                serde_json::json!({"name": result.name, "status": status, "exit_code": result.exit_code}),
                            );
                            setup_results.push(result);
                            if status == "error" {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        emit_step(
                            &task.id,
                            "setup_command_finished",
                            serde_json::json!({"name": spec.name, "status": "error", "error": e.to_string()}),
                        );
                        setup_error = Some(e);
                        break;
                    }
                }
            }

            match setup_error {
                Some(e) => {
                    let reason = format!("Setup commands error: {e}");
                    tracing::warn!(task_id = %task.short_id, error = %e, "Lifecycle: setup command error");
                    let task_repo =
                        TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                    let _ = task_repo
                        .transition(
                            &task.id,
                            (runtime_role.config().release_action)(),
                            "agent-supervisor",
                            "system",
                            Some(&reason),
                            None,
                        )
                        .await;
                    teardown_worktree(
                        &task.short_id,
                        &worktree_path,
                        &project_dir,
                        &app_state,
                        false,
                    )
                    .await;
                    return_free!();
                }
                None => {
                    crate::actors::slot::commands::log_commands_run_event(
                        &task.id,
                        "setup",
                        &setup_specs,
                        &setup_results,
                        &app_state,
                    )
                    .await;
                    let failed = setup_results.iter().find(|r| r.exit_code != 0);
                    if let Some(failure) = failed {
                        let reason = format!(
                            "Setup command '{}' failed (exit {})\nstdout: {}\nstderr: {}",
                            failure.name,
                            failure.exit_code,
                            failure.stdout.trim(),
                            failure.stderr.trim(),
                        );
                        tracing::warn!(
                            task_id = %task.short_id,
                            command = %failure.name,
                            "Lifecycle: setup command failed; releasing task"
                        );
                        let task_repo =
                            TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
                        let _ = task_repo
                            .transition(
                                &task.id,
                                (runtime_role.config().release_action)(),
                                "agent-supervisor",
                                "system",
                                Some(&reason),
                                None,
                            )
                            .await;
                        teardown_worktree(
                            &task.short_id,
                            &worktree_path,
                            &project_dir,
                            &app_state,
                            false,
                        )
                        .await;
                        return_free!();
                    }
                    tracing::info!(
                        task_id = %task.short_id,
                        duration_ms = setup_start.elapsed().as_millis(),
                        "Lifecycle: setup commands completed"
                    );
                }
            }
        }
        // Format verification_rules as a markdown list for the prompt.
        // Each rule becomes: "- `<pattern>`: `cmd1`, `cmd2`"
        let prompt_verification_rules = if verification_rules.is_empty() {
            None
        } else {
            let formatted = verification_rules
                .iter()
                .map(|r| {
                    let cmds = r
                        .commands
                        .iter()
                        .map(|c| format!("`{c}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("- `{}`: {}", r.pattern, cmds)
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(formatted)
        };
        (
            prompt_setup_commands,
            prompt_verification_commands,
            prompt_verification_rules,
        )
    };

    let conflict_files = conflict_ctx.as_ref().map(|m| {
        m.conflicting_files
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n")
    });

    // Fetch activity log for the prompt: last 3 high-signal comments plus a
    // summary of total counts by role so the agent knows what to look up.
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let activity_entries = task_repo.list_activity(&task.id).await.ok();
    let activity_text = match &activity_entries {
        Some(entries) if !entries.is_empty() => {
            // Last 3 high-signal comments (lead, reviewer, verification)
            let feedback = recent_feedback(entries, 3);

            // Count comments by role for the summary line
            let mut counts: std::collections::BTreeMap<&str, usize> =
                std::collections::BTreeMap::new();
            for e in entries {
                if e.event_type == "comment" {
                    *counts.entry(e.actor_role.as_str()).or_default() += 1;
                }
            }
            let count_summary: String = counts
                .iter()
                .map(|(role, n)| format!("{n} {role}"))
                .collect::<Vec<_>>()
                .join(", ");

            let mut parts = Vec::new();
            if !feedback.is_empty() {
                parts.push(format!(
                    "**Recent feedback (newest last):**\n{}",
                    feedback.join("\n\n---\n")
                ));
            }
            if !count_summary.is_empty() {
                parts.push(format!(
                    "**Activity totals:** {count_summary} comments. Use `task_activity_list` with `actor_role` filter for full history."
                ));
            }

            if parts.is_empty() {
                None
            } else {
                Some(parts.join("\n\n"))
            }
        }
        _ => None,
    };

    // Extract worker submission summary/concerns and last verification failure
    // from the activity log so the reviewer can see why certain changes were made.
    let (worker_summary, worker_concerns, verification_failure) =
        extract_worker_context(&activity_entries);

    // ── Build epic context for roles that need it (e.g. lead) ─────────────────
    let epic_context = if role.needs_epic_context() {
        if let Some(ref epic_id) = task.epic_id {
            let epic_repo =
                djinn_db::EpicRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            let task_repo_ctx =
                TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
            match epic_repo.get(epic_id).await {
                Ok(Some(epic)) => {
                    let mut ctx_lines = vec![
                        format!("**Epic:** {} ({})", epic.title, epic.short_id),
                        format!("**Description:** {}", epic.description),
                        format!("**Memory refs:** {}", epic.memory_refs),
                    ];
                    // Load sibling tasks
                    if let Ok(result) = task_repo_ctx
                        .list_filtered(djinn_db::ListQuery {
                            parent: Some(epic_id.clone()),
                            limit: 50,
                            ..Default::default()
                        })
                        .await
                    {
                        let open = result.tasks.iter().filter(|t| t.status != "closed").count();
                        let closed = result.tasks.iter().filter(|t| t.status == "closed").count();
                        ctx_lines.push(format!(
                            "\n### Sibling Tasks ({open} open, {closed} closed)"
                        ));
                        for t in &result.tasks {
                            let status_marker = if t.status == "closed" {
                                "closed"
                            } else {
                                &t.status
                            };
                            ctx_lines
                                .push(format!("- [{}] {}: {}", status_marker, t.short_id, t.title));
                        }
                    }
                    Some(ctx_lines.join("\n"))
                }
                _ => None,
            }
        } else {
            None
        }
    } else {
        None
    };

    // ── Build knowledge context from scope-matched notes ─────────────
    let knowledge_context = {
        let note_repo =
            djinn_db::NoteRepository::new(app_state.db.clone(), app_state.event_bus.clone());

        let task_paths = derive_task_scope_paths(&task, epic_context.as_deref());

        match note_repo
            .query_by_scope_overlap(
                &task.project_id,
                &task_paths,
                &["pattern", "pitfall", "case"],
                0.3,
                10,
            )
            .await
        {
            Ok(notes) if !notes.is_empty() => Some(format_knowledge_notes(&notes, 2000)),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(
                    task_id = %task.short_id,
                    error = %e,
                    "Lifecycle: failed to query knowledge context"
                );
                None
            }
        }
    };

    let base_system_prompt = runtime_role.render_prompt(
        &task,
        &TaskContext {
            project_path: project_path.clone(),
            workspace_path: worktree_path.display().to_string(),
            diff: None,
            commits: None,
            start_commit: None,
            end_commit: None,
            conflict_files,
            merge_base_branch: conflict_ctx.as_ref().map(|m| m.base_branch.clone()),
            merge_target_branch: conflict_ctx.as_ref().map(|m| m.merge_target.clone()),
            merge_failure_context: merge_validation_ctx,
            setup_commands: prompt_setup_commands.clone(),
            verification_commands: prompt_verification_commands.clone(),
            verification_rules: prompt_verification_rules.clone(),
            activity: activity_text,
            worker_summary,
            worker_concerns,
            verification_failure,
            epic_context,
            knowledge_context,
        },
    );
    // Apply role-level prompt extensions from DB (system_prompt_extensions + learned_prompt).
    let system_prompt_with_extensions = crate::prompts::apply_role_extensions(
        &base_system_prompt,
        &system_prompt_extensions,
        learned_prompt.as_deref(),
    );
    // Append skills section after all other extensions.
    let system_prompt =
        crate::prompts::apply_skills(&system_prompt_with_extensions, &resolved_skills);

    let context_window = app_state
        .catalog
        .find_model(&model_id)
        .map(|m| m.context_window)
        .unwrap_or(0);

    let session_repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    // Use the resume session ID or a pre-generated UUID as the provider
    // affinity key.  The actual DB session record is created later (once we
    // know whether we're resuming or starting fresh) to avoid orphaning a
    // ghost record when the second creation shadows this one.
    let affinity_key = resume_record_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    // ── Build Djinn-native provider ───────────────────────────────────────────
    #[cfg(test)]
    let provider: Box<dyn LlmProvider> = if let Some(p) = provider_override {
        // Wrap the Arc in a Box so the type matches the non-test path.
        struct ArcProvider(Arc<dyn LlmProvider>);
        use crate::provider::{StreamEvent, ToolChoice};
        use std::pin::Pin;
        impl LlmProvider for ArcProvider {
            fn name(&self) -> &str {
                self.0.name()
            }
            fn stream<'a>(
                &'a self,
                conv: &'a djinn_provider::message::Conversation,
                tools: &'a [serde_json::Value],
                tool_choice: Option<ToolChoice>,
            ) -> Pin<
                Box<
                    dyn futures::Future<
                            Output = anyhow::Result<
                                Pin<
                                    Box<
                                        dyn futures::Stream<Item = anyhow::Result<StreamEvent>>
                                            + Send,
                                    >,
                                >,
                            >,
                        > + Send
                        + 'a,
                >,
            > {
                self.0.stream(conv, tools, tool_choice)
            }
        }
        Box::new(ArcProvider(p))
    } else {
        let cred = provider_credential
            .expect("provider_credential must be Some when provider_override is None");
        let telemetry_meta = build_telemetry_meta(runtime_role.config().name, &task_id);
        let provider_config = match cred {
            ProviderCredential::OAuthConfig(mut cfg) => {
                cfg.model_id = model_name.clone();
                cfg.context_window = context_window.max(0) as u32;
                cfg.telemetry = Some(telemetry_meta);
                cfg.session_affinity_key = Some(affinity_key.clone());
                *cfg
            }
            ProviderCredential::ApiKey(_key_name, api_key) => {
                let format_family = format_family_for_provider(&catalog_provider_id, &model_name);
                let base_url = app_state
                    .catalog
                    .list_providers()
                    .iter()
                    .find(|p| p.id == catalog_provider_id)
                    .map(|p| p.base_url.clone())
                    .filter(|u| !u.is_empty())
                    .unwrap_or_else(|| default_base_url(&catalog_provider_id));
                crate::provider::ProviderConfig {
                    base_url,
                    auth: auth_method_for_provider(&catalog_provider_id, &api_key),
                    format_family,
                    model_id: model_name.clone(),
                    context_window: context_window.max(0) as u32,
                    telemetry: Some(telemetry_meta),
                    session_affinity_key: Some(affinity_key.clone()),
                    provider_headers: Default::default(),
                    capabilities: capabilities_for_provider(&catalog_provider_id),
                }
            }
        };
        create_provider(provider_config)
    };
    #[cfg(not(test))]
    let provider: Box<dyn LlmProvider> = {
        let telemetry_meta = build_telemetry_meta(runtime_role.config().name, &task_id);
        let cred =
            provider_credential.expect("provider_credential must be Some in non-test builds");
        let provider_config = match cred {
            ProviderCredential::OAuthConfig(mut cfg) => {
                cfg.model_id = model_name.clone();
                cfg.context_window = context_window.max(0) as u32;
                cfg.telemetry = Some(telemetry_meta);
                cfg.session_affinity_key = Some(affinity_key.clone());
                *cfg
            }
            ProviderCredential::ApiKey(_key_name, api_key) => {
                let format_family = format_family_for_provider(&catalog_provider_id, &model_name);
                let base_url = app_state
                    .catalog
                    .list_providers()
                    .iter()
                    .find(|p| p.id == catalog_provider_id)
                    .map(|p| p.base_url.clone())
                    .filter(|u| !u.is_empty())
                    .unwrap_or_else(|| default_base_url(&catalog_provider_id));
                crate::provider::ProviderConfig {
                    base_url,
                    auth: auth_method_for_provider(&catalog_provider_id, &api_key),
                    format_family,
                    model_id: model_name.clone(),
                    context_window: context_window.max(0) as u32,
                    telemetry: Some(telemetry_meta),
                    session_affinity_key: Some(affinity_key.clone()),
                    provider_headers: Default::default(),
                    capabilities: capabilities_for_provider(&catalog_provider_id),
                }
            }
        };
        create_provider(provider_config)
    };

    // ── Create or resume session record + build conversation ─────────────────
    let mut tools = (runtime_role.config().tool_schemas)();
    let builtin_count = tools.len();

    // Append MCP-provided tool schemas to the session tool list.
    if let Some(ref registry) = mcp_registry {
        let mcp_schemas = registry.tool_schemas();
        tracing::info!(
            task_id = %task.short_id,
            role = %runtime_role.config().name,
            mcp_tool_count = mcp_schemas.len(),
            "Lifecycle: appending MCP tool schemas to session"
        );
        tools.extend_from_slice(mcp_schemas);
    }

    // ADR-048 §2C: Sort tool schemas deterministically for prompt-cache
    // stability.  Built-in tools stay first (sorted among themselves),
    // followed by MCP tools (sorted among themselves).
    sort_tool_schemas(&mut tools, builtin_count);

    // Workers include recent feedback in the initial message; other roles use
    // a generic kickoff (they read activity via tools themselves).
    let fresh_user_message = runtime_role
        .initial_user_message(&task_id, &app_state)
        .await;

    // Try to resume from a paused session's saved conversation.
    emit_step(
        &task.id,
        "session_creating",
        serde_json::json!({"resume": resume_record_id.is_some()}),
    );
    let (current_record_id, mut conversation) = if let Some(ref resume_id) = resume_record_id {
        match super::conversation_store::load(resume_id).await {
            Ok(Some(mut saved_conv)) => {
                // Replace the system prompt with a fresh one (reflects updated AC).
                if !saved_conv.messages.is_empty()
                    && saved_conv.messages[0].role == crate::message::Role::System
                {
                    saved_conv.messages[0] = Message::system(system_prompt.clone());
                }

                // Compact the prior conversation before appending feedback.
                // This strips the model's "I'm done" messages and frees context
                // window for actual work, while preserving research/context.
                let pre_compact_len = saved_conv.messages.len();
                let compacted = crate::compaction::compact_conversation(
                    provider.as_ref(),
                    &mut saved_conv,
                    resume_id,
                    &task_id,
                    &app_state,
                    crate::compaction::CompactionContext::PreResume(
                        runtime_role.config().name.to_string(),
                    ),
                    context_window,
                )
                .await;
                tracing::info!(
                    task_id = %task_id,
                    session_record_id = %resume_id,
                    pre_compact_len,
                    post_compact_len = saved_conv.messages.len(),
                    compacted,
                    "Lifecycle: compacted conversation before resume"
                );

                // Append reviewer feedback as the fresh user message.
                let feedback = resume_context_for_task(&task_id, &app_state).await;
                saved_conv.push(Message::user(feedback));

                // Reuse the paused session record.
                session_repo.set_running(resume_id).await.ok();
                tracing::info!(
                    task_id = %task_id,
                    session_record_id = %resume_id,
                    conversation_len = saved_conv.messages.len(),
                    "Lifecycle: resumed paused session with reviewer feedback"
                );
                (Some(resume_id.clone()), saved_conv)
            }
            Ok(None) | Err(_) => {
                // Conversation file missing/corrupt — fall back to fresh session.
                tracing::warn!(
                    task_id = %task_id,
                    session_record_id = %resume_id,
                    "Lifecycle: conversation file missing; falling back to fresh session"
                );
                // Mark the stale paused session as interrupted.
                let _ = session_repo
                    .update(resume_id, SessionStatus::Interrupted, 0, 0)
                    .await;
                let record_id = match session_repo
                    .create(CreateSessionParams {
                        project_id: &task.project_id,
                        task_id: Some(&task.id),
                        model: &model_id,
                        agent_type: runtime_role.config().name,
                        worktree_path: worktree_path.to_str(),
                        metadata_json: None,
                    })
                    .await
                {
                    Ok(r) => Some(r.id),
                    Err(e) => {
                        tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create session record");
                        transition_interrupted(
                            &task_id,
                            runtime_role.config().release_action,
                            &e.to_string(),
                            &app_state,
                        )
                        .await;
                        teardown_worktree(
                            &task.short_id,
                            &worktree_path,
                            &project_dir,
                            &app_state,
                            false,
                        )
                        .await;
                        return_free!();
                    }
                };
                let mut conv = Conversation::new();
                conv.push(Message::system(system_prompt.clone()));
                conv.push(Message::user(fresh_user_message.clone()));
                (record_id, conv)
            }
        }
    } else {
        // Fresh session — no paused session to resume.
        let record_id = match session_repo
            .create(CreateSessionParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                model: &model_id,
                agent_type: role.config().name,
                worktree_path: worktree_path.to_str(),
                metadata_json: None,
            })
            .await
        {
            Ok(r) => Some(r.id),
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to create session record");
                transition_interrupted(
                    &task_id,
                    runtime_role.config().release_action,
                    &e.to_string(),
                    &app_state,
                )
                .await;
                teardown_worktree(
                    &task.short_id,
                    &worktree_path,
                    &project_dir,
                    &app_state,
                    false,
                )
                .await;
                return_free!();
            }
        };
        let mut conv = Conversation::new();
        conv.push(Message::system(system_prompt.clone()));
        conv.push(Message::user(fresh_user_message));
        (record_id, conv)
    };

    // Use the DB record ID as the session ID so OTel/Langfuse traces, error
    // diagnostics, and session_messages all reference the same identifier.
    let current_session_id = current_record_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    // ── Run reply loop ────────────────────────────────────────────────────────
    let (reply_result, final_output, tokens_in_loop, tokens_out_loop) = run_reply_loop(
        ReplyLoopContext {
            provider: provider.as_ref(),
            tools: &tools,
            task_id: &task.id,
            task_short_id: &task.short_id,
            session_id: &current_session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: role.config().name,
            finalize_tool_names: role.config().finalize_tool_names,
            context_window,
            model_id: &model_id,
            cancel: &cancel,
            global_cancel: &pause,
            app_state: &app_state,
            mcp_registry: mcp_registry.as_ref(),
        },
        &mut conversation,
        resume_record_id.is_some(),
    )
    .await;

    // Persist conversation messages to session_messages table for timeline display.
    // Compaction already saves pre-compaction messages; this saves whatever remains
    // (post-compaction turns, or the full conversation if no compaction occurred).
    if let Some(ref record_id) = current_record_id {
        let msg_repo = djinn_db::SessionMessageRepository::new(
            app_state.db.clone(),
            app_state.event_bus.clone(),
        );
        if let Err(e) = msg_repo
            .insert_messages_batch(record_id, &task.id, &conversation.messages)
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to persist conversation messages to DB");
        }
    }

    // Commit a WIP snapshot for interrupted / non-worker sessions.
    // Skip this for successful worker sessions — they get a proper commit
    // with the message from submit_work in commit_final_work_if_needed below.
    let worker_completed_ok = reply_result.is_ok() && runtime_role.config().preserves_session;
    if !worker_completed_ok {
        commit_wip_if_needed(&task_id, &worktree_path, &app_state).await;
    }

    // ── Shut down LSP clients for this worktree ──────────────────────────────
    // Centralized cleanup: every session-end path flows through here, so LSP
    // processes are always reclaimed regardless of how the session finished
    // (normal completion, error, timeout, pause, cancellation).
    let lsp_killed = app_state.lsp.shutdown_for_worktree(&worktree_path).await;
    if lsp_killed > 0 {
        tracing::info!(
            task_id = %task_id,
            worktree = %worktree_path.display(),
            clients_killed = lsp_killed,
            "Lifecycle: shut down LSP clients on session end"
        );
    }

    // ── Handle pause/kill cancellation ────────────────────────────────────────
    if pause.is_cancelled() {
        tracing::info!(task_id = %task_id, "Lifecycle: paused; preserving worktree");
        update_session_record_paused(
            current_record_id.as_deref(),
            tokens_in_loop,
            tokens_out_loop,
            &app_state,
        )
        .await;
        // LSP clients already shut down above (centralized cleanup).
        return_free!();
    }
    if cancel.is_cancelled() {
        tracing::info!(task_id = %task_id, "Lifecycle: cancelled; preserving worktree for retry");
        update_session_record(
            current_record_id.as_deref(),
            SessionStatus::Interrupted,
            tokens_in_loop,
            tokens_out_loop,
            &app_state,
        )
        .await;
        // Preserve the worktree — the task will be released back to open and
        // re-dispatched, so the next session can reuse the build cache.
        cleanup_worktree(&task_id, &worktree_path, &app_state).await;
        transition_interrupted(
            &task_id,
            runtime_role.config().release_action,
            "session cancelled",
            &app_state,
        )
        .await;
        return_killed!();
    }

    let final_result = reply_result;
    let tokens_in = tokens_in_loop;
    let tokens_out = tokens_out_loop;

    // ── Post-loop: health + transitions + cleanup ─────────────────────────────

    // Health tracking.
    match &final_result {
        Ok(()) => app_state.health_tracker.record_success(&model_id),
        Err(_) => app_state.health_tracker.record_failure(&model_id),
    }
    app_state.persist_model_health_state().await;

    let is_worker_done = final_result.is_ok() && role.config().preserves_session;

    // Worktree: commit final work.  For workers, preserve the worktree and
    // save the conversation so the session can be resumed after review.
    // Non-workers (reviewers, lead) still clean up immediately.
    if is_worker_done {
        let commit_msg = if final_output
            .finalize_tool_name
            .as_deref()
            .is_some_and(|n| n == "submit_work")
        {
            let payload = final_output.finalize_payload.as_ref();
            let title = payload
                .and_then(|p| p.get("commit_title"))
                .and_then(|s| s.as_str());
            let body = payload
                .and_then(|p| p.get("summary"))
                .and_then(|s| s.as_str());
            format_commit_message(title, body)
        } else {
            None
        };
        if let Err(e) =
            commit_final_work_if_needed(&task_id, &worktree_path, &app_state, commit_msg.as_deref())
                .await
        {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "Lifecycle: failed to commit final work"
            );
        }
    }
    if is_worker_done {
        // Save conversation for potential resume after review cycle.
        if let Some(ref record_id) = current_record_id
            && let Err(e) = super::conversation_store::save(record_id, &conversation).await
        {
            tracing::warn!(
                task_id = %task_id,
                record_id = %record_id,
                error = %e,
                "Lifecycle: failed to save conversation for resume"
            );
        }
        // Mark session as Paused (not Completed) — worker may resume.
        update_session_record_paused(
            current_record_id.as_deref(),
            tokens_in,
            tokens_out,
            &app_state,
        )
        .await;
        // Don't clean up worktree — will be reused on resume.
        // LSP clients already shut down above (centralized cleanup).

        // Spawn extraction for completed worker sessions too — reflection must
        // happen regardless of whether the session is preserved for resume.
        if let Some(ref record_id) = current_record_id {
            let session_id_for_extraction = record_id.clone();
            let session_id_for_llm = record_id.clone();
            let messages_snapshot = conversation.messages.clone();
            let app_state_for_extraction = app_state.clone();
            let app_state_for_llm = app_state.clone();
            tokio::spawn(async move {
                let taxonomy = super::session_extraction::run_structural_extraction(
                    session_id_for_extraction,
                    messages_snapshot,
                    app_state_for_extraction,
                )
                .await;
                // LLM knowledge extraction after structural extraction
                if let Some(taxonomy) = taxonomy {
                    super::llm_extraction::run_llm_extraction(
                        session_id_for_llm,
                        taxonomy,
                        app_state_for_llm,
                    )
                    .await;
                }
            });
        }
    } else {
        // Non-worker or failed: close session and clean up.
        let session_status = if final_result.is_ok() {
            SessionStatus::Completed
        } else {
            SessionStatus::Failed
        };
        update_session_record(
            current_record_id.as_deref(),
            session_status,
            tokens_in,
            tokens_out,
            &app_state,
        )
        .await;

        // Spawn structural extraction as a background job for completed sessions,
        // then chain LLM knowledge extraction after it.
        if session_status == SessionStatus::Completed
            && let Some(ref record_id) = current_record_id
        {
            let session_id_for_extraction = record_id.clone();
            let session_id_for_llm = record_id.clone();
            let messages_snapshot = conversation.messages.clone();
            let app_state_for_extraction = app_state.clone();
            let app_state_for_llm = app_state.clone();
            tokio::spawn(async move {
                let taxonomy = super::session_extraction::run_structural_extraction(
                    session_id_for_extraction,
                    messages_snapshot,
                    app_state_for_extraction,
                )
                .await;
                // LLM knowledge extraction after structural extraction
                if let Some(taxonomy) = taxonomy {
                    super::llm_extraction::run_llm_extraction(
                        session_id_for_llm,
                        taxonomy,
                        app_state_for_llm,
                    )
                    .await;
                }
            });
        }

        // Commit any file changes before tearing down the worktree so the
        // branch preserves them for the PR pipeline.
        if final_result.is_ok() {
            let commit_msg = {
                let payload = final_output.finalize_payload.as_ref();
                let title = payload
                    .and_then(|p| p.get("commit_title"))
                    .and_then(|s| s.as_str());
                let body = payload
                    .and_then(|p| p.get("summary"))
                    .and_then(|s| s.as_str());
                format_commit_message(title, body)
            };
            if let Err(e) = commit_final_work_if_needed(
                &task_id,
                &worktree_path,
                &app_state,
                commit_msg.as_deref(),
            )
            .await
            {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "Lifecycle: failed to commit non-worker final work"
                );
            }
        }

        // Preserve the worktree for potential re-dispatch.  Non-worker roles
        // (lead, reviewer, architect) may release the task back to open, in
        // which case the next worker session benefits from the existing
        // target/ build cache.  Real teardown happens on task close/merge
        // (task_merge.rs) or via purge_all_worktrees on execution restart.
        cleanup_worktree(&task_id, &worktree_path, &app_state).await;

        // For non-worker roles, free the slot immediately and run
        // post-session work (finalize payload, on_complete, transition) in a
        // background task.  This prevents slow operations like merge
        // verification from blocking a slot while no LLM session is active.
        let final_error = final_result.as_ref().err().map(|e| e.to_string());
        let final_result_ok = final_result.is_ok();
        spawn_post_session_work(PostSessionParams {
            task_id: task_id.clone(),
            project_path: project_path.clone(),
            role: role.clone(),
            app_state: app_state.clone(),
            final_output,
            final_result_ok,
            final_error,
            tokens_in,
            tokens_out,
        });
        return_free!();
    }

    // ── Worker path: inline post-session (workers don't do merges) ────────────

    // Log reviewer feedback from text markers — only when no finalize payload is
    // present. With ADR-036, reviewer feedback comes via submit_review.feedback
    // and is logged by process_finalize_payload below.
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if final_output.finalize_payload.is_none()
        && let Some(feedback) = final_output.reviewer_feedback.as_deref()
    {
        let payload = serde_json::json!({ "body": feedback }).to_string();
        if let Err(e) = task_repo
            .log_activity(
                Some(&task_id),
                "agent-supervisor",
                "reviewer",
                "comment",
                &payload,
            )
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "failed to store reviewer feedback comment");
        }
    }

    // Process finalize tool payload (ADR-036): log structured activity and apply
    // side effects (e.g. AC updates for submit_review) before on_complete runs.
    if final_result.is_ok() {
        super::finalize_handlers::process_finalize_payload(
            &final_output.finalize_payload,
            final_output.finalize_tool_name.as_deref().unwrap_or(""),
            &task_id,
            &app_state,
        )
        .await;
    }

    // Log session errors.
    if let Err(reason) = &final_result {
        let payload = serde_json::json!({
            "error": reason.to_string(),
            "agent_type": role.config().name,
        })
        .to_string();
        let _ = task_repo
            .log_activity(
                Some(&task_id),
                "agent-supervisor",
                "system",
                "session_error",
                &payload,
            )
            .await;
    }
    if final_result.is_ok()
        && let Some(reason) = final_output.runtime_error.as_deref()
    {
        let payload = serde_json::json!({
            "error": reason,
            "agent_type": role.config().name,
        })
        .to_string();
        let _ = task_repo
            .log_activity(
                Some(&task_id),
                "agent-supervisor",
                "system",
                "session_error",
                &payload,
            )
            .await;
    }

    // Determine transition.
    let transition = match final_result {
        Ok(()) => role.on_complete(&task_id, &final_output, &app_state).await,
        Err(reason) => Some(((role.config().release_action)(), Some(reason.to_string()))),
    };

    apply_transition_and_dispatch(
        transition,
        &task_id,
        &project_path,
        &role,
        &app_state,
        tokens_in,
        tokens_out,
    )
    .await;

    return_free!();
}

// ─── Background post-session work (non-worker roles) ─────────────────────────

/// Parameters for the background post-session task that runs after the slot is
/// freed.  Handles finalize payload processing, on_complete (which may do slow
/// merge + verification), transition, and dispatch triggering.
struct PostSessionParams {
    task_id: String,
    project_path: String,
    role: Arc<dyn AgentRole>,
    app_state: AgentContext,
    final_output: crate::output_parser::ParsedAgentOutput,
    final_result_ok: bool,
    final_error: Option<String>,
    tokens_in: i64,
    tokens_out: i64,
}

/// Spawn the post-session work as a background tokio task so the slot is freed
/// immediately after the LLM session ends.
fn spawn_post_session_work(params: PostSessionParams) {
    // Register in the verification tracker so the coordinator's stuck-task
    // recovery doesn't reset the task while post-session work (merge,
    // transition) is still in flight.
    params.app_state.register_verification(&params.task_id);
    tokio::spawn(async move {
        let PostSessionParams {
            task_id,
            project_path,
            role,
            app_state,
            final_output,
            final_result_ok,
            final_error,
            tokens_in,
            tokens_out,
        } = params;

        // Log reviewer feedback from text markers.
        let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
        if final_output.finalize_payload.is_none()
            && let Some(feedback) = final_output.reviewer_feedback.as_deref()
        {
            let payload = serde_json::json!({ "body": feedback }).to_string();
            if let Err(e) = task_repo
                .log_activity(
                    Some(&task_id),
                    "agent-supervisor",
                    "task_reviewer",
                    "comment",
                    &payload,
                )
                .await
            {
                tracing::warn!(task_id = %task_id, error = %e, "failed to store reviewer feedback comment");
            }
        }

        // Process finalize tool payload (ADR-036).
        if final_result_ok {
            super::finalize_handlers::process_finalize_payload(
                &final_output.finalize_payload,
                final_output.finalize_tool_name.as_deref().unwrap_or(""),
                &task_id,
                &app_state,
            )
            .await;
        }

        // Log session errors.
        if let Some(reason) = &final_error {
            let payload = serde_json::json!({
                "error": reason,
                "agent_type": role.config().name,
            })
            .to_string();
            let _ = task_repo
                .log_activity(
                    Some(&task_id),
                    "agent-supervisor",
                    "system",
                    "session_error",
                    &payload,
                )
                .await;
        }
        if final_result_ok && let Some(reason) = final_output.runtime_error.as_deref() {
            let payload = serde_json::json!({
                "error": reason,
                "agent_type": role.config().name,
            })
            .to_string();
            let _ = task_repo
                .log_activity(
                    Some(&task_id),
                    "agent-supervisor",
                    "system",
                    "session_error",
                    &payload,
                )
                .await;
        }

        // Determine transition.
        let transition = if final_result_ok {
            role.on_complete(&task_id, &final_output, &app_state).await
        } else if let Some(reason) = final_error {
            Some(((role.config().release_action)(), Some(reason)))
        } else {
            Some(((role.config().release_action)(), None))
        };

        apply_transition_and_dispatch(
            transition,
            &task_id,
            &project_path,
            &role,
            &app_state,
            tokens_in,
            tokens_out,
        )
        .await;

        // Deregister from the verification tracker now that all post-session
        // work (finalize payload, on_complete, transition, merge) is done.
        app_state.deregister_verification(&task_id);
    });
}

/// Apply the transition from on_complete and trigger dispatch for the project.
/// Shared by both the inline worker path and the background non-worker path.
async fn apply_transition_and_dispatch(
    transition: Option<(TransitionAction, Option<String>)>,
    task_id: &str,
    project_path: &str,
    role: &Arc<dyn AgentRole>,
    app_state: &AgentContext,
    tokens_in: i64,
    tokens_out: i64,
) {
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    if let Some((action, reason)) = transition {
        tracing::info!(
            task_id = %task_id,
            role = %role.config().name,
            transition_action = ?action,
            transition_reason = reason.as_deref().unwrap_or("<none>"),
            tokens_in,
            tokens_out,
            "Lifecycle: applying session transition"
        );
        let is_conflict_rejection = action == TransitionAction::TaskReviewRejectConflict;
        let is_submit_verification = action == TransitionAction::SubmitVerification;
        // If the session died because the persisted conversation has an
        // orphaned tool_call / function_call (typically after a mid-turn
        // interruption — db lock, idle kill, prior crash), the OpenAI
        // Responses API rejects every replay with HTTP 400. Resuming will
        // hit the same wall forever, so drop the paused session record and
        // let the next dispatch start fresh against the same worktree.
        let is_orphaned_tool_call = reason
            .as_deref()
            .map(is_orphaned_tool_call_error_str)
            .unwrap_or(false);
        if is_orphaned_tool_call {
            tracing::warn!(
                task_id = %task_id,
                "Lifecycle: dropping poisoned session due to orphaned tool call; next dispatch will start a fresh session"
            );
        }
        if let Err(e) = retry_task_transition_on_locked(|| async {
            task_repo
                .transition(
                    task_id,
                    action.clone(),
                    "agent-supervisor",
                    "system",
                    reason.as_deref(),
                    None,
                )
                .await
        })
        .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "Lifecycle: failed to transition task after session");
            // If the intended transition failed (e.g. Close blocked by
            // downstream blockers), fall back to Release so the task doesn't
            // get stuck in in_progress with no session — which causes the
            // coordinator's stuck-task recovery to loop indefinitely.
            if action != TransitionAction::Release {
                let fallback_reason = format!("Fallback release: {e}");
                if let Err(e2) = retry_task_transition_on_locked(|| async {
                    task_repo
                        .transition(
                            task_id,
                            TransitionAction::Release,
                            "agent-supervisor",
                            "system",
                            Some(&fallback_reason),
                            None,
                        )
                        .await
                })
                .await
                {
                    // Release is only valid from in_progress.  If the task is
                    // already in open/todo/closed (e.g. a concurrent session or
                    // background handler transitioned it), that's fine — the
                    // task isn't stuck.  Log at warn instead of error to avoid
                    // noisy false alarms.
                    tracing::warn!(
                        task_id = %task_id,
                        error = %e2,
                        "Lifecycle: fallback Release failed (task likely already transitioned)"
                    );
                }
            }
        }
        if is_conflict_rejection || is_orphaned_tool_call {
            interrupt_paused_worker_session(task_id, app_state).await;
        }
        if is_submit_verification {
            super::verification::spawn_verification(
                task_id.to_string(),
                project_path.to_string(),
                app_state.clone(),
            );
        }
    } else {
        tracing::info!(
            task_id = %task_id,
            role = %role.config().name,
            tokens_in,
            tokens_out,
            "Lifecycle: session completed with no task transition"
        );
    }

    // Trigger dispatcher for the project so the next ready task starts promptly.
    if let Ok(task) = load_task(task_id, app_state).await
        && let Some(coordinator) = app_state.coordinator().await
    {
        let _ = coordinator
            .trigger_dispatch_for_project(&task.project_id)
            .await;
    }
}
