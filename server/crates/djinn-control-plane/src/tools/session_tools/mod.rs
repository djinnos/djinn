use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use djinn_core::extension_diagnostics::ExtensionLoadDiagnosticV1;
use djinn_core::models::SessionRecord;
use djinn_db::ActivityQuery;
use djinn_db::ExtensionLoadDiagnosticRepository;
use djinn_db::SessionMessageRepository;
use djinn_db::SessionRepository;
use djinn_db::TaskRepository;

use super::memory_tools::MemoryRevisionEvent;
use super::memory_tools::revision_readers::{
    SessionDiffSelector, caller_has_note_read_permission, session_revision_events,
};

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionListParams {
    /// Task UUID or short_id.
    pub task_id: Option<String>,
    /// Optional project hint (slug or UUID). Only needed to disambiguate a
    /// `short_id`; a task UUID resolves globally without it.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionActiveParams {
    /// Absolute project path (required).
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionShowParams {
    /// Session UUID.
    pub id: String,
    /// Absolute project path (required).
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SessionMessagesParams {
    /// Session UUID.
    pub id: String,
    /// Absolute project path (required).
    pub project: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionMessage {
    pub role: String,
    pub content: Vec<super::json_object::AnyJson>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionMessagesResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<SessionMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionToolSession {
    pub id: String,
    /// `None` for chat sessions (global, user-scoped); `Some(_)` for every
    /// other agent type. See `SessionRecord::project_id`.
    pub project_id: Option<String>,
    pub task_id: Option<String>,
    pub model_id: String,
    pub agent_type: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    /// Running totals of prompt-cache reads (hits) and writes (creation).
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    /// Optional deliberate-park reason for terminal sessions, e.g. `budget`.
    pub parked_reason: Option<String>,
    /// Authoritative workspace path for the session, sourced from the
    /// attached `task_run` (via `sessions.task_run_id`). `None` when the
    /// session is not attached to a task run or the run has no recorded
    /// workspace.
    pub workspace_path: Option<String>,
}

impl From<SessionRecord> for SessionToolSession {
    fn from(value: SessionRecord) -> Self {
        // Synchronous boundary: no DB access, so `workspace_path` is left
        // unset. Callers that need it should use `from_session_with_run`.
        Self {
            id: value.id,
            project_id: value.project_id,
            task_id: value.task_id,
            model_id: value.model_id,
            agent_type: value.agent_type,
            started_at: value.started_at,
            ended_at: value.ended_at,
            status: value.status,
            tokens_in: value.tokens_in,
            tokens_out: value.tokens_out,
            cache_read_tokens: value.cache_read_tokens,
            cache_write_tokens: value.cache_write_tokens,
            parked_reason: value.parked_reason,
            workspace_path: None,
        }
    }
}

impl SessionToolSession {
    /// Build a `SessionToolSession` by resolving `workspace_path` from
    /// `task_runs` via `task_run_id`. `None` when the session has no run.
    pub async fn from_session_with_run(
        value: SessionRecord,
        task_run_repo: &djinn_db::repositories::task_run::TaskRunRepository,
    ) -> Self {
        let workspace_path = match value.task_run_id.as_deref() {
            Some(run_id) => task_run_repo
                .get(run_id)
                .await
                .ok()
                .flatten()
                .and_then(|run| run.workspace_path),
            None => None,
        };
        Self {
            id: value.id,
            project_id: value.project_id,
            task_id: value.task_id,
            model_id: value.model_id,
            agent_type: value.agent_type,
            started_at: value.started_at,
            ended_at: value.ended_at,
            status: value.status,
            tokens_in: value.tokens_in,
            tokens_out: value.tokens_out,
            cache_read_tokens: value.cache_read_tokens,
            cache_write_tokens: value.cache_write_tokens,
            parked_reason: value.parked_reason,
            workspace_path,
        }
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionListResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionToolSession>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionActiveResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionToolSession>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_sessions: Option<Vec<SessionToolSession>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_triggered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct SessionShowResponse {
    #[serde(flatten)]
    pub session: Option<SessionToolSession>,
    /// Canonical extension-load failures associated with this session. Present
    /// on successful lookups, including as an empty array.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<Vec<super::json_object::AnyJson>>")]
    pub extension_load_diagnostics: Option<Vec<ExtensionLoadDiagnosticV1>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Task timeline (single-call) ──────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskTimelineParams {
    /// Task UUID or short_id.
    pub task_id: Option<String>,
    /// Optional project hint (slug or UUID). Only needed to disambiguate a
    /// `short_id`; a task UUID resolves globally without it.
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TimelineMessage {
    pub session_id: String,
    pub role: String,
    pub content: Vec<super::json_object::AnyJson>,
    pub agent_type: String,
    pub model_id: String,
    pub timestamp: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TimelineActivity {
    pub event_type: String,
    pub actor_role: String,
    /// Renderer-friendly discriminator. Usually mirrors `event_type`; for
    /// structured payloads this is the semantic timeline kind.
    pub kind: String,
    pub payload: super::json_object::AnyJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<super::json_object::AnyJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub timestamp: String,
}

/// A canonical extension-load diagnostic placed on a task timeline.
///
/// The diagnostic remains the shared V1 wire object rather than a timeline-
/// specific copy of its fields. `session_id` and `timestamp` provide the
/// placement metadata needed by timeline renderers.
#[derive(Serialize, schemars::JsonSchema)]
pub struct TimelineExtensionLoadDiagnosticEvent {
    /// Fixed discriminator for extension-load diagnostic timeline events.
    pub kind: String,
    pub session_id: String,
    pub timestamp: String,
    #[schemars(with = "super::json_object::AnyJson")]
    pub diagnostic: ExtensionLoadDiagnosticV1,
}

fn render_timeline_activity(e: &djinn_core::models::ActivityEntry) -> TimelineActivity {
    let payload_value: serde_json::Value =
        serde_json::from_str(&e.payload).unwrap_or_else(|_| serde_json::json!({}));
    let (kind, details, summary) =
        super::task_tools::ops::render_activity_metadata(&e.event_type, &payload_value);

    TimelineActivity {
        event_type: e.event_type.clone(),
        actor_role: e.actor_role.clone(),
        kind,
        payload: super::json_object::AnyJson(payload_value),
        details,
        summary,
        timestamp: e.created_at.clone(),
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskTimelineResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionToolSession>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<TimelineMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<Vec<TimelineActivity>>,
    /// Immutable memory revisions caused by this task's sessions, newest-first
    /// by `(created_at, revision_id)`. Bodies follow the same authorization
    /// and redaction decision as `memory_session_diff`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_revision_events: Option<Vec<MemoryRevisionEvent>>,
    /// Canonical session-associated extension-load diagnostics, including as
    /// an empty array on successful timeline lookups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extension_load_diagnostic_events: Option<Vec<TimelineExtensionLoadDiagnosticEvent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DjinnMcpServer {
    /// Resolve a task for the read-only session views (`task_timeline`,
    /// `session_list`). The Kanban is a cross-team board spanning many
    /// projects, so a task's sessions must be reachable regardless of which
    /// project the caller currently has selected. We resolve the task
    /// GLOBALLY by id (a UUID is globally unique) and the callers then scope
    /// every downstream query to the task's OWN `project_id` — never the
    /// caller-supplied one. An optional `project` hint is honored first so a
    /// `short_id` (unique only within a project) still resolves unambiguously
    /// for callers that pass it (e.g. agents); it is otherwise ignored.
    async fn resolve_task_for_session_view(
        &self,
        task_repo: &TaskRepository,
        task_id: &str,
        project_hint: Option<&str>,
    ) -> Result<Option<djinn_core::models::Task>, String> {
        if let Some(proj) = project_hint.filter(|p| !p.is_empty())
            && let Ok(project_id) = self.resolve_project_id(proj).await
            && let Some(task) = task_repo
                .resolve_in_project(&project_id, task_id)
                .await
                .map_err(|e| e.to_string())?
        {
            return Ok(Some(task));
        }
        task_repo.resolve(task_id).await.map_err(|e| e.to_string())
    }
}

#[tool_router(router = session_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// List all sessions for a task, newest first by started_at.
    #[tool(
        description = "session_list(task_id) returns all sessions for a task ordered by started_at"
    )]
    pub async fn session_list(
        &self,
        Parameters(p): Parameters<SessionListParams>,
    ) -> Json<SessionListResponse> {
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let task_id = match p.task_id.as_deref() {
            Some(id) => id,
            None => {
                return Json(SessionListResponse {
                    task_id: None,
                    sessions: None,
                    error: Some("task_id is required".to_string()),
                });
            }
        };
        let task = match self
            .resolve_task_for_session_view(&task_repo, task_id, p.project.as_deref())
            .await
        {
            Ok(Some(t)) => t,
            Ok(None) => {
                return Json(SessionListResponse {
                    task_id: None,
                    sessions: None,
                    error: Some(format!("task not found: {}", task_id)),
                });
            }
            Err(e) => {
                return Json(SessionListResponse {
                    task_id: None,
                    sessions: None,
                    error: Some(e),
                });
            }
        };
        // Scope to the task's OWN project — the board is cross-project.
        let project_id = task.project_id.clone();

        let repo = SessionRepository::new(self.state.db().clone(), self.state.event_bus());
        let task_run_repo =
            djinn_db::repositories::task_run::TaskRunRepository::new(self.state.db().clone());
        match repo.list_for_task_in_project(&project_id, &task.id).await {
            Ok(sessions) => {
                let mut out = Vec::with_capacity(sessions.len());
                for s in sessions {
                    out.push(SessionToolSession::from_session_with_run(s, &task_run_repo).await);
                }
                Json(SessionListResponse {
                    task_id: Some(task.id),
                    sessions: Some(out),
                    error: None,
                })
            }
            Err(e) => Json(SessionListResponse {
                task_id: None,
                sessions: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// List currently running sessions across all tasks.
    #[tool(description = "session_active() returns all currently running sessions across tasks")]
    pub async fn session_active(
        &self,
        Parameters(p): Parameters<SessionActiveParams>,
    ) -> Json<SessionActiveResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(SessionActiveResponse {
                    sessions: None,
                    stale_sessions: None,
                    recovery_triggered: None,
                    error: Some(e),
                });
            }
        };
        let Some(pool) = self.state.pool().await else {
            return Json(SessionActiveResponse {
                sessions: None,
                stale_sessions: None,
                recovery_triggered: None,
                error: Some("slot pool actor not initialized".to_string()),
            });
        };
        let coordinator = self.state.coordinator().await;
        let repo = SessionRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.list_active_in_project(&project_id).await {
            Ok(sessions) => {
                let mut runtime_sessions = Vec::new();
                let mut stale_sessions = Vec::new();
                let now = time::OffsetDateTime::now_utc();
                let config = djinn_core::liveness::LivenessConfig::OBSERVATION;

                for session in sessions {
                    if let Some(task_id) = session.task_id.as_deref() {
                        match pool.has_session(task_id).await {
                            Ok(true) => {
                                // Slot is alive — but it might be wedged
                                // on a never-returning model call, or
                                // running past the model runtime cap
                                // (epic 5wxi: glm-5.2 worker cap).
                                // Classify using the combined
                                // interruption helper so both paths are
                                // handled uniformly.
                                let last_msg =
                                    repo.last_message_at(&session.id).await.unwrap_or(None);
                                let interruption =
                                    djinn_core::liveness::classify_session_interruption(
                                        &session.agent_type,
                                        &session.model_id,
                                        &session.started_at,
                                        last_msg.as_deref(),
                                        session.tokens_in,
                                        session.tokens_out,
                                        now,
                                        &config,
                                        &djinn_core::liveness::RuntimeCapConfig::default_config(),
                                    );
                                match interruption {
                                    None => {
                                        runtime_sessions.push(session);
                                    }
                                    Some(reason) => {
                                        if let djinn_core::liveness::SessionInterruptReason::RuntimeCap {
                                            ref provider_id,
                                            ref model_name,
                                            ..
                                        } = reason
                                        {
                                            tracing::info!(
                                                session_id = %session.id,
                                                provider_id = %provider_id,
                                                model_name = %model_name,
                                                "session_active: glm-5.2 runtime-cap \
                                                 session flagged for reroute \
                                                 (not a task-quality strike)"
                                            );
                                        }
                                        stale_sessions.push(session);
                                    }
                                }
                            }
                            Ok(false) => stale_sessions.push(session),
                            Err(e) => {
                                return Json(SessionActiveResponse {
                                    sessions: None,
                                    stale_sessions: None,
                                    recovery_triggered: None,
                                    error: Some(e.to_string()),
                                });
                            }
                        }
                    } else {
                        runtime_sessions.push(session);
                    }
                }

                let mut recovery_triggered = false;
                if !stale_sessions.is_empty()
                    && let Some(coordinator) = coordinator
                    && coordinator
                        .trigger_dispatch_for_project(&project_id)
                        .await
                        .is_ok()
                {
                    recovery_triggered = true;
                }

                let task_run_repo = djinn_db::repositories::task_run::TaskRunRepository::new(
                    self.state.db().clone(),
                );
                let mut runtime_out = Vec::with_capacity(runtime_sessions.len());
                for s in runtime_sessions {
                    runtime_out
                        .push(SessionToolSession::from_session_with_run(s, &task_run_repo).await);
                }
                let mut stale_out = Vec::with_capacity(stale_sessions.len());
                for s in stale_sessions {
                    stale_out
                        .push(SessionToolSession::from_session_with_run(s, &task_run_repo).await);
                }
                Json(SessionActiveResponse {
                    sessions: Some(runtime_out),
                    stale_sessions: Some(stale_out),
                    recovery_triggered: Some(recovery_triggered),
                    error: None,
                })
            }
            Err(e) => Json(SessionActiveResponse {
                sessions: None,
                stale_sessions: None,
                recovery_triggered: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Get a single session by id.
    #[tool(
        description = "session_show(id) returns session details and canonical session-associated extension_load_diagnostics: id, task_id, model_id, agent_type, started_at, ended_at, status, tokens_in, tokens_out, cache_read_tokens, cache_write_tokens"
    )]
    pub async fn session_show(
        &self,
        Parameters(p): Parameters<SessionShowParams>,
    ) -> Json<SessionShowResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(SessionShowResponse {
                    session: None,
                    extension_load_diagnostics: None,
                    error: Some(e),
                });
            }
        };
        let repo = SessionRepository::new(self.state.db().clone(), self.state.event_bus());
        let task_run_repo =
            djinn_db::repositories::task_run::TaskRunRepository::new(self.state.db().clone());
        match repo.get_in_project(&project_id, &p.id).await {
            Ok(Some(session)) => {
                let diagnostic_repo =
                    ExtensionLoadDiagnosticRepository::new(self.state.db().clone());
                match diagnostic_repo
                    .list_for_session(&project_id, &session.id)
                    .await
                {
                    Ok(extension_load_diagnostics) => Json(SessionShowResponse {
                        session: Some(
                            SessionToolSession::from_session_with_run(session, &task_run_repo)
                                .await,
                        ),
                        extension_load_diagnostics: Some(extension_load_diagnostics),
                        error: None,
                    }),
                    Err(e) => Json(SessionShowResponse {
                        session: None,
                        extension_load_diagnostics: None,
                        error: Some(e.to_string()),
                    }),
                }
            }
            Ok(None) => Json(SessionShowResponse {
                session: None,
                extension_load_diagnostics: None,
                error: Some(format!("session not found: {}", p.id)),
            }),
            Err(e) => Json(SessionShowResponse {
                session: None,
                extension_load_diagnostics: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Return full conversation messages for a session (from DB).
    #[tool(
        description = "session_messages(id) returns conversation messages for a session: role + content blocks (text/tool_use/tool_result/thinking)"
    )]
    pub async fn session_messages(
        &self,
        Parameters(p): Parameters<SessionMessagesParams>,
    ) -> Json<SessionMessagesResponse> {
        let err = |e: String| {
            Json(SessionMessagesResponse {
                session_id: None,
                agent_type: None,
                model_id: None,
                messages: None,
                error: Some(e),
            })
        };

        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return err(e),
        };

        let session_repo = SessionRepository::new(self.state.db().clone(), self.state.event_bus());
        let session = match session_repo.get_in_project(&project_id, &p.id).await {
            Ok(Some(s)) => s,
            Ok(None) => return err(format!("session not found: {}", p.id)),
            Err(e) => return err(e.to_string()),
        };

        let msg_repo =
            SessionMessageRepository::new(self.state.db().clone(), self.state.event_bus());
        let conversation = match msg_repo.load_conversation(&session.id).await {
            Ok(c) => c,
            Err(e) => {
                return Json(SessionMessagesResponse {
                    session_id: Some(session.id),
                    agent_type: Some(session.agent_type),
                    model_id: Some(session.model_id),
                    messages: None,
                    error: Some(format!("failed to load messages: {e}")),
                });
            }
        };

        let messages: Vec<SessionMessage> = conversation
            .messages
            .into_iter()
            .map(|msg| {
                let role = match msg.role {
                    djinn_provider::message::Role::System => "system",
                    djinn_provider::message::Role::User => "user",
                    djinn_provider::message::Role::Assistant => "assistant",
                }
                .to_owned();
                let content = msg
                    .content
                    .into_iter()
                    .map(|block| {
                        super::json_object::AnyJson(serde_json::to_value(block).unwrap_or_default())
                    })
                    .collect();
                SessionMessage { role, content }
            })
            .collect();

        Json(SessionMessagesResponse {
            session_id: Some(session.id),
            agent_type: Some(session.agent_type),
            model_id: Some(session.model_id),
            messages: Some(messages),
            error: None,
        })
    }

    /// Return the full timeline for a task: sessions, messages, and activity — in one call.
    #[tool(
        description = "task_timeline(task_id) returns sessions, all conversation messages (with timestamps), and activity log entries for a task in a single call."
    )]
    pub async fn task_timeline(
        &self,
        Parameters(p): Parameters<TaskTimelineParams>,
    ) -> Json<TaskTimelineResponse> {
        let err = |e: String| {
            Json(TaskTimelineResponse {
                sessions: None,
                messages: None,
                activity: None,
                memory_revision_events: None,
                extension_load_diagnostic_events: None,
                error: Some(e),
            })
        };

        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let task_id = match p.task_id.as_deref() {
            Some(id) => id,
            None => return err("task_id is required".to_string()),
        };
        let task = match self
            .resolve_task_for_session_view(&task_repo, task_id, p.project.as_deref())
            .await
        {
            Ok(Some(t)) => t,
            Ok(None) => return err(format!("task not found: {}", task_id)),
            Err(e) => return err(e),
        };
        // Scope every downstream query to the task's OWN project — never the
        // caller's selected project (which differs on a cross-project board).
        let project_id = task.project_id.clone();

        let session_repo = SessionRepository::new(self.state.db().clone(), self.state.event_bus());
        let msg_repo =
            SessionMessageRepository::new(self.state.db().clone(), self.state.event_bus());

        // 1. Get sessions
        let sessions = match session_repo
            .list_for_task_in_project(&project_id, &task.id)
            .await
        {
            Ok(s) => s,
            Err(e) => return err(e.to_string()),
        };

        let session_ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();

        // Reuse the exact memory session-diff service (including cursor and
        // body projection logic) rather than querying the ledger here. The
        // timeline retains its single-call contract by exhausting bounded
        // pages for each task-owned session and merging their ordered streams.
        let redact_bodies = !caller_has_note_read_permission(self).await;
        let mut memory_revision_events = Vec::new();
        for session_id in &session_ids {
            let mut before_cursor = None;
            loop {
                let (mut events, next_cursor) = match session_revision_events(
                    self,
                    &project_id,
                    SessionDiffSelector::Session(session_id),
                    super::memory_tools::SESSION_DIFF_LIMIT_MAX as usize,
                    before_cursor.as_deref(),
                    redact_bodies,
                )
                .await
                {
                    Ok(page) => page,
                    Err(e) => return err(e),
                };
                memory_revision_events.append(&mut events);
                before_cursor = next_cursor;
                if before_cursor.is_none() {
                    break;
                }
            }
        }
        memory_revision_events.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.revision_id.cmp(&left.revision_id))
        });

        // Diagnostics are task-owned and must be read under the task's owning
        // project. Keep only rows associated with one of the sessions already
        // returned by this timeline: this excludes doctor-only rows and stale
        // or otherwise unrelated session associations.
        let diagnostic_repo = ExtensionLoadDiagnosticRepository::new(self.state.db().clone());
        let extension_load_diagnostic_events =
            match diagnostic_repo.list_for_task(&project_id, &task.id).await {
                Ok(diagnostics) => diagnostics
                    .into_iter()
                    .filter_map(|diagnostic| {
                        let session_id = diagnostic.session_id.clone()?;
                        session_ids.contains(&session_id).then(|| {
                            TimelineExtensionLoadDiagnosticEvent {
                                kind: "extension_load_diagnostic".to_owned(),
                                session_id,
                                timestamp: diagnostic.last_seen_at.clone(),
                                diagnostic,
                            }
                        })
                    })
                    .collect(),
                Err(e) => return err(e.to_string()),
            };

        // Build a lookup map: session_id → (agent_type, model_id)
        let session_info: std::collections::HashMap<String, (&str, &str)> = sessions
            .iter()
            .map(|s| (s.id.clone(), (s.agent_type.as_str(), s.model_id.as_str())))
            .collect();

        // 2. Bulk-load messages for all sessions (1 query)
        let raw_messages = match msg_repo.load_for_sessions(&session_ids).await {
            Ok(m) => m,
            Err(e) => return err(e.to_string()),
        };

        let messages: Vec<TimelineMessage> = raw_messages
            .into_iter()
            .filter(|(_, role, _, _)| role != "system") // skip system prompts
            .map(|(session_id, role, content_json, created_at)| {
                let content: Vec<super::json_object::AnyJson> =
                    serde_json::from_str::<Vec<super::json_object::AnyJson>>(&content_json)
                        .unwrap_or_default();
                let (agent_type, model_id) = session_info
                    .get(&session_id)
                    .copied()
                    .unwrap_or(("unknown", "unknown"));
                TimelineMessage {
                    session_id,
                    role,
                    content,
                    agent_type: agent_type.to_owned(),
                    model_id: model_id.to_owned(),
                    timestamp: created_at,
                }
            })
            .collect();

        // 3. Get activity log entries
        let q = ActivityQuery {
            project_id: Some(project_id),
            task_id: Some(task.id),
            event_type: None,
            actor_role: None,
            from_time: None,
            to_time: None,
            limit: 500,
            offset: 0,
        };
        let activity = match task_repo.query_activity(q).await {
            Ok(entries) => entries.iter().map(render_timeline_activity).collect(),
            Err(e) => return err(e.to_string()),
        };

        let task_run_repo =
            djinn_db::repositories::task_run::TaskRunRepository::new(self.state.db().clone());
        let mut sessions_out = Vec::with_capacity(sessions.len());
        for s in sessions {
            sessions_out.push(SessionToolSession::from_session_with_run(s, &task_run_repo).await);
        }
        Json(TaskTimelineResponse {
            sessions: Some(sessions_out),
            messages: Some(messages),
            activity: Some(activity),
            memory_revision_events: Some(memory_revision_events),
            extension_load_diagnostic_events: Some(extension_load_diagnostic_events),
            error: None,
        })
    }
}
