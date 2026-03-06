use std::path::Path;
use std::sync::Arc;

use goose::agents::{
    Agent as GooseAgent, AgentConfig as GooseAgentConfig, GoosePlatform,
};
use goose::config::{GooseMode, PermissionManager};
use goose::model::ModelConfig;
use goose::providers;

use crate::agent::{AgentType, SessionManager, SessionType};
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::models::session::SessionStatus;
use crate::server::AppState;

use super::*;

pub(super) struct CompactResult {
    pub(super) new_session_id: String,
    pub(super) new_record_id: String,
    pub(super) agent: Arc<GooseAgent>,
    pub(super) kickoff_summary: String,
}

/// Performs context compaction inline (without actor messaging). Creates a new
/// Goose session with a summary of the old one, and returns the new session info.
#[allow(clippy::too_many_arguments)]
pub(super) async fn compact_inline(
    task_id: &str,
    agent_type: AgentType,
    project_id: &str,
    old_session_id: &str,
    old_record_id: Option<&str>,
    model_id: &str,
    goose_provider_id: &str,
    model_name: &str,
    worktree_path: &Path,
    context_window: i64,
    tokens_in: i64,
    session_manager: &Arc<SessionManager>,
    app_state: &AppState,
    resume_context: Option<&str>,
) -> Result<CompactResult, String> {
    // 1. Read conversation history, final token counts, and extension data from old session.
    let (final_tokens_in, final_tokens_out, messages, old_extension_data) = match session_manager
        .get_session(old_session_id, true)
        .await
    {
        Ok(s) => {
            let tin = s.accumulated_input_tokens.or(s.input_tokens).unwrap_or(0) as i64;
            let tout = s.accumulated_output_tokens.or(s.output_tokens).unwrap_or(0) as i64;
            let msgs = s
                .conversation
                .map(|c| c.messages().clone())
                .unwrap_or_default();
            (tin.max(tokens_in), tout, msgs, Some(s.extension_data))
        }
        Err(e) => {
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to read Goose session");
            (tokens_in, 0, vec![], None)
        }
    };

    // 2. Finalize old Djinn session record.
    if let Some(record_id) = old_record_id {
        let repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
        if let Err(e) = repo
            .update(
                record_id,
                SessionStatus::Compacted,
                final_tokens_in,
                final_tokens_out,
            )
            .await
        {
            tracing::warn!(record_id = %record_id, error = %e, "compaction: failed to finalize old session record");
        }
    }

    let goose_model = ModelConfig::new(model_name)
        .map_err(|e| format!("compaction: failed to build ModelConfig: {e}"))?
        .with_canonical_limits(goose_provider_id);

    // 3. Generate summary.
    //
    // Trim messages to fit within the model's context window.  We target 80%
    // of the window (leaving room for the compaction system prompt + output).
    // If the session used more tokens than that, we keep only the leading
    // fraction of messages and drop the tail.
    let trimmed_messages = if !messages.is_empty() && context_window > 0 && final_tokens_in > 0 {
        let target = (context_window as f64 * 0.80) as i64;
        if final_tokens_in > target {
            let keep_ratio = target as f64 / final_tokens_in as f64;
            let keep_count = ((messages.len() as f64 * keep_ratio).ceil() as usize).max(1);
            tracing::info!(
                task_id = %task_id,
                total_messages = messages.len(),
                keep_count,
                final_tokens_in,
                target,
                "compaction: trimming conversation to fit context window"
            );
            messages[..keep_count].to_vec()
        } else {
            messages
        }
    } else {
        messages
    };

    let summary = if trimmed_messages.is_empty() {
        tracing::warn!(task_id = %task_id, "compaction: empty conversation history; using fallback summary");
        "Context window was compacted. Please review the current state of the worktree and continue the task.".to_string()
    } else {
        let compaction_system = crate::agent::prompts::render_compaction_prompt();
        let summary_provider = providers::create(goose_provider_id, goose_model.clone(), vec![])
            .await
            .map_err(|e| {
                app_state.health_tracker().record_failure(model_id);
                format!("compaction: summary provider creation failed: {e}")
            })?;
        let model_config = summary_provider.get_model_config();
        summary_provider
            .complete(
                &model_config,
                old_session_id,
                compaction_system,
                &trimmed_messages,
                &[],
            )
            .await
            .map(|(msg, _)| {
                tracing::info!(task_id = %task_id, "compaction: summary generated successfully");
                msg.as_concat_text()
            })
            .map_err(|e| format!("compaction: summary generation failed: {e}"))?
    };

    // 4. Create new Goose session.
    let task_name = {
        let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
        match task_repo.get(task_id).await {
            Ok(Some(t)) => format!("{} {} (compacted)", t.short_id, t.title),
            _ => format!("{task_id} (compacted)"),
        }
    };
    let new_goose_session = session_manager
        .create_session(worktree_path.to_owned(), task_name, SessionType::SubAgent)
        .await
        .map_err(|e| format!("compaction: failed to create new Goose session: {e}"))?;

    // 4b. Carry over extension_data (todo state, etc.) from old session.
    if let Some(ext_data) = old_extension_data
        && let Err(e) = session_manager
            .update(&new_goose_session.id)
            .extension_data(ext_data)
            .apply()
            .await
    {
        tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to carry over extension_data");
    }

    // 5. Create new Djinn session record.
    let session_repo = SessionRepository::new(app_state.db().clone(), app_state.events().clone());
    let new_record = session_repo
        .create(
            project_id,
            task_id,
            model_id,
            agent_type.as_str(),
            worktree_path.to_str(),
            Some(&new_goose_session.id),
            old_record_id,
        )
        .await
        .map_err(|e| format!("compaction: failed to create new session record: {e}"))?;

    // Log compaction activity.
    {
        let task_repo = TaskRepository::new(app_state.db().clone(), app_state.events().clone());
        let usage_pct = if context_window > 0 {
            final_tokens_in as f64 / context_window as f64
        } else {
            0.0
        };
        let payload = serde_json::json!({
            "old_session_id": old_record_id.unwrap_or(""),
            "new_session_id": new_record.id,
            "tokens_in_at_compaction": final_tokens_in,
            "context_window": context_window,
            "usage_pct": usage_pct,
            "summary_token_count": summary.chars().count(),
        })
        .to_string();
        if let Err(e) = task_repo
            .log_activity(Some(task_id), "system", "system", "compaction", &payload)
            .await
        {
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to log activity");
        }
    }

    // 6. Set up new agent.
    let extensions = extensions_for(agent_type);
    let provider = providers::create(goose_provider_id, goose_model, extensions.clone())
        .await
        .map_err(|e| {
            app_state.health_tracker().record_failure(model_id);
            format!("compaction: failed to create new agent provider: {e}")
        })?;

    let agent = Arc::new(GooseAgent::with_config(GooseAgentConfig::new(
        session_manager.clone(),
        PermissionManager::instance(),
        None,
        GooseMode::Auto,
        true,
        GoosePlatform::GooseCli,
    )));

    agent
        .update_provider(provider, &new_goose_session.id)
        .await
        .map_err(|e| {
            app_state.health_tracker().record_failure(model_id);
            format!("compaction: failed to set provider on new agent: {e}")
        })?;

    for ext in extensions {
        if let Err(e) = agent.add_extension(ext, &new_goose_session.id).await {
            tracing::warn!(task_id = %task_id, error = %e, "compaction: failed to add extension");
        }
    }

    let kickoff_summary = match resume_context {
        Some(ctx) => format!("{summary}\n\n---\n\n{ctx}"),
        None => summary,
    };

    Ok(CompactResult {
        new_session_id: new_goose_session.id,
        new_record_id: new_record.id,
        agent,
        kickoff_summary,
    })
}
