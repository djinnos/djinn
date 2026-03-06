// AgentSupervisor — 1x global, manages in-process Goose session lifecycle.

mod dispatch;
mod helpers;
mod run_loop;
mod session_mgmt;
mod session_ops;
mod tokens;

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Once;
use std::time::{Duration, Instant};

use goose::config::Config as GooseConfig;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::actors::slot::SlotEvent;
use crate::agent::{AgentType, GooseSessionHandle, SessionManager};
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::TaskRepository;
use crate::models::session::SessionStatus;
use crate::models::task::{Task, TransitionAction};
use crate::server::AppState;
static GOOSE_BUILTINS_REGISTERED: Once = Once::new();

pub(super) fn register_goose_builtin_extensions() {
    GOOSE_BUILTINS_REGISTERED.call_once(|| {
        let builtins: HashMap<&'static str, goose::builtin_extension::SpawnServerFn> =
            goose_mcp::BUILTIN_EXTENSIONS
                .iter()
                .map(|(name, spawn)| (*name, *spawn))
                .collect();
        goose::builtin_extension::register_builtin_extensions(builtins);
    });
}

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("actor channel closed")]
    ActorDead,
    #[error("no response from actor")]
    NoResponse,
    #[error("session already active for task {task_id}")]
    SessionAlreadyActive { task_id: String },
    #[error("task {task_id} not found")]
    TaskNotFound { task_id: String },
    #[error("invalid model id '{model_id}', expected provider/model")]
    InvalidModelId { model_id: String },
    #[error("no credential stored for provider {provider_id} (expected key {key_name})")]
    MissingCredential {
        provider_id: String,
        key_name: String,
    },
    #[error("task transition failed for {task_id}: {reason}")]
    TaskTransitionFailed { task_id: String, reason: String },
    #[error("goose session failed: {0}")]
    Goose(String),
    #[error("model {model_id} at capacity ({active}/{max})")]
    ModelAtCapacity {
        model_id: String,
        active: u32,
        max: u32,
    },
    #[error("paused session stale for task {task_id} (worktree missing)")]
    PausedSessionStale { task_id: String },
}

#[derive(Debug, Clone)]
pub struct SupervisorStatus {
    pub active_sessions: usize,
    pub capacity: HashMap<String, ModelCapacity>,
    pub running_sessions: Vec<RunningSessionInfo>,
}

#[derive(Debug, Clone)]
pub struct ModelCapacity {
    pub active: u32,
    pub max: u32,
}

#[derive(Debug, Clone)]
pub struct RunningSessionInfo {
    pub task_id: String,
    pub model_id: String,
    pub session_id: String,
    pub duration_seconds: u64,
    pub worktree_path: Option<String>,
}

pub(super) type Reply<T> = oneshot::Sender<Result<T, SupervisorError>>;

pub(super) enum SupervisorMessage {
    Dispatch {
        task_id: String,
        project_path: String,
        model_id: String,
        respond_to: Reply<()>,
    },
    HasSession {
        task_id: String,
        respond_to: Reply<bool>,
    },
    KillSession {
        task_id: String,
        respond_to: Reply<()>,
    },
    PauseSession {
        task_id: String,
        respond_to: Reply<()>,
    },
    InterruptAll {
        reason: String,
        respond_to: Reply<()>,
    },
    InterruptProject {
        project_id: String,
        reason: String,
        respond_to: Reply<()>,
    },
    GetStatus {
        respond_to: Reply<SupervisorStatus>,
    },
    GetSessionForTask {
        task_id: String,
        respond_to: Reply<Option<RunningSessionInfo>>,
    },
    UpdateSessionLimits {
        max_sessions: HashMap<String, u32>,
        default_max: u32,
        respond_to: Reply<()>,
    },
}

pub(super) struct LifecycleHandle {
    pub(super) join: tokio::task::JoinHandle<anyhow::Result<()>>,
    pub(super) kill: CancellationToken,
    pub(super) pause: CancellationToken,
    pub(super) model_id: String,
    pub(super) project_id: String,
    pub(super) started_at: Instant,
}

pub(super) struct AgentSupervisor {
    pub(super) receiver: mpsc::Receiver<SupervisorMessage>,
    pub(super) slot_event_rx: mpsc::Receiver<SlotEvent>,
    pub(super) slot_event_tx: mpsc::Sender<SlotEvent>,
    pub(super) lifecycle_handles: HashMap<String, LifecycleHandle>,
    pub(super) sessions: HashMap<String, GooseSessionHandle>,
    pub(super) capacity: HashMap<String, ModelCapacity>,
    pub(super) session_models: HashMap<String, String>,
    pub(super) session_agent_types: HashMap<String, AgentType>,
    pub(super) session_projects: HashMap<String, String>,
    pub(super) task_session_records: HashMap<String, String>,
    pub(super) interrupted_sessions: HashSet<String>,
    /// Tasks fully owned by the supervisor: from dispatch start through post-session
    /// completion (verification, commit, transition, cleanup). Prevents stuck detection
    /// from false-positiving during the sessionless post-session window.
    pub(super) in_flight: HashSet<String>,
    pub(super) default_max_sessions: u32,
    pub(super) configured_model_limits: HashMap<String, u32>,
    pub(super) session_manager: Arc<SessionManager>,
    pub(super) app_state: AppState,
    pub(super) cancel: CancellationToken,
}

impl AgentSupervisor {
    pub(super) fn new(
        receiver: mpsc::Receiver<SupervisorMessage>,
        app_state: AppState,
        session_manager: Arc<SessionManager>,
        cancel: CancellationToken,
    ) -> Self {
        register_goose_builtin_extensions();
        let (slot_event_tx, slot_event_rx) = mpsc::channel(64);
        // Disable Goose's built-in auto-compaction so Djinn owns the compaction lifecycle entirely.
        // check_if_compaction_needed() returns false when threshold <= 0.0 || threshold >= 1.0.
        if let Err(e) = GooseConfig::global().set_param("GOOSE_AUTO_COMPACT_THRESHOLD", 0.0f64) {
            tracing::warn!(error = %e, "Failed to disable Goose auto-compaction threshold");
        }
        Self {
            receiver,
            slot_event_rx,
            slot_event_tx,
            lifecycle_handles: HashMap::new(),
            sessions: HashMap::new(),
            capacity: HashMap::new(),
            session_models: HashMap::new(),
            session_agent_types: HashMap::new(),
            session_projects: HashMap::new(),
            task_session_records: HashMap::new(),
            interrupted_sessions: HashSet::new(),
            in_flight: HashSet::new(),
            default_max_sessions: 1,
            configured_model_limits: HashMap::new(),
            session_manager,
            app_state,
            cancel,
        }
    }

    pub(super) fn max_for_model(&self, model_id: &str) -> u32 {
        self.configured_model_limits
            .get(model_id)
            .copied()
            .unwrap_or(self.default_max_sessions)
    }

    pub(super) fn apply_session_limits(
        &mut self,
        max_sessions: HashMap<String, u32>,
        default_max: u32,
    ) {
        self.default_max_sessions = default_max.max(1);
        self.configured_model_limits = max_sessions
            .into_iter()
            .filter(|(_, max)| *max > 0)
            .collect();

        for (model_id, max) in &self.configured_model_limits {
            let entry = self
                .capacity
                .entry(model_id.clone())
                .or_insert(ModelCapacity {
                    active: 0,
                    max: *max,
                });
            entry.max = *max;
        }

        let configured = self.configured_model_limits.clone();
        let default_max = self.default_max_sessions;
        for (model_id, entry) in &mut self.capacity {
            entry.max = configured.get(model_id).copied().unwrap_or(default_max);
        }
    }

    pub(super) fn spawn_mock_session(&mut self, task_id: String, model_id: String) {
        let session_cancel = CancellationToken::new();
        let session_cancel_for_join = session_cancel.clone();
        let global_cancel = self.cancel.clone();

        let join = tokio::spawn(async move {
            tokio::select! {
                _ = session_cancel_for_join.cancelled() => {}
                _ = global_cancel.cancelled() => {}
            }
            Ok(())
        });

        self.sessions.insert(
            task_id.clone(),
            GooseSessionHandle {
                join,
                cancel: session_cancel,
                session_id: format!("mock-session-{task_id}"),
                task_id: task_id.clone(),
                worktree_path: None,
                started_at: Instant::now(),
            },
        );
        self.session_models.insert(task_id.clone(), model_id);
        self.session_agent_types.insert(task_id, AgentType::Worker);
    }
}

pub(super) struct PendingInterrupt {
    pub(super) task_id: String,
    pub(super) join: tokio::task::JoinHandle<anyhow::Result<()>>,
    pub(super) worktree_path: Option<PathBuf>,
    pub(super) model_id: Option<String>,
    pub(super) agent_type: AgentType,
    pub(super) goose_session_id: String,
    pub(super) session_record_id: Option<String>,
}

#[derive(Clone)]
pub struct AgentSupervisorHandle {
    sender: mpsc::Sender<SupervisorMessage>,
}

impl AgentSupervisorHandle {
    pub fn spawn(
        app_state: AppState,
        session_manager: Arc<SessionManager>,
        cancel: CancellationToken,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(32);
        tokio::spawn(AgentSupervisor::new(receiver, app_state, session_manager, cancel).run());
        Self { sender }
    }

    async fn request<T>(
        &self,
        f: impl FnOnce(Reply<T>) -> SupervisorMessage,
    ) -> Result<T, SupervisorError> {
        let (tx, rx) = oneshot::channel();
        self.sender
            .send(f(tx))
            .await
            .map_err(|_| SupervisorError::ActorDead)?;
        rx.await.map_err(|_| SupervisorError::NoResponse)?
    }

    pub async fn has_session(&self, task_id: &str) -> Result<bool, SupervisorError> {
        self.request(|tx| SupervisorMessage::HasSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn dispatch(
        &self,
        task_id: &str,
        project_path: &str,
        model_id: &str,
    ) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::Dispatch {
            task_id: task_id.to_owned(),
            project_path: project_path.to_owned(),
            model_id: model_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn kill_session(&self, task_id: &str) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::KillSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn pause_session(&self, task_id: &str) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::PauseSession {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn get_status(&self) -> Result<SupervisorStatus, SupervisorError> {
        self.request(|tx| SupervisorMessage::GetStatus { respond_to: tx })
            .await
    }

    pub async fn session_for_task(
        &self,
        task_id: &str,
    ) -> Result<Option<RunningSessionInfo>, SupervisorError> {
        self.request(|tx| SupervisorMessage::GetSessionForTask {
            task_id: task_id.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn interrupt_project(
        &self,
        project_id: &str,
        reason: &str,
    ) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::InterruptProject {
            project_id: project_id.to_owned(),
            reason: reason.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn interrupt_all(&self, reason: &str) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::InterruptAll {
            reason: reason.to_owned(),
            respond_to: tx,
        })
        .await
    }

    pub async fn update_session_limits(
        &self,
        max_sessions: HashMap<String, u32>,
        default_max: u32,
    ) -> Result<(), SupervisorError> {
        self.request(|tx| SupervisorMessage::UpdateSessionLimits {
            max_sessions,
            default_max: default_max.max(1),
            respond_to: tx,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use tempfile::TempDir;

    use super::*;
    use crate::agent::init_session_manager;
    use crate::test_helpers;

    fn spawn_supervisor(temp: &TempDir) -> AgentSupervisorHandle {
        let db = test_helpers::create_test_db();
        let cancel = CancellationToken::new();
        let app_state = AppState::new(db, cancel.clone());
        let sessions = init_session_manager(temp.path().to_path_buf());
        AgentSupervisorHandle::spawn(app_state, sessions, cancel)
    }

    #[tokio::test]
    async fn tracks_session_lifecycle() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);
        let project_path = temp.path().to_str().unwrap();

        assert!(!supervisor.has_session("task-1").await.unwrap());
        supervisor
            .dispatch("task-1", project_path, "test/mock")
            .await
            .unwrap();
        assert!(supervisor.has_session("task-1").await.unwrap());

        supervisor.kill_session("task-1").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        assert!(!supervisor.has_session("task-1").await.unwrap());
    }

    #[tokio::test]
    async fn enforces_per_model_capacity() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);
        let project_path = temp.path().to_str().unwrap();

        supervisor
            .dispatch("task-1", project_path, "test/mock")
            .await
            .unwrap();
        let err = supervisor
            .dispatch("task-2", project_path, "test/mock")
            .await
            .unwrap_err();
        assert!(matches!(err, SupervisorError::ModelAtCapacity { .. }));

        supervisor.kill_session("task-1").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        supervisor
            .dispatch("task-2", project_path, "test/mock")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn status_reports_capacity_and_active_count() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);
        let project_path = temp.path().to_str().unwrap();

        supervisor
            .dispatch("task-1", project_path, "test/mock")
            .await
            .unwrap();
        let status = supervisor.get_status().await.unwrap();
        assert_eq!(status.active_sessions, 1);
        let model = status.capacity.get("test/mock").unwrap();
        assert_eq!(model.active, 1);
        assert_eq!(model.max, 1);
    }

    #[tokio::test]
    async fn applies_per_model_session_limits_from_settings() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);
        let project_path = temp.path().to_str().unwrap();

        let mut limits = HashMap::new();
        limits.insert("test/mock".to_string(), 4);
        limits.insert("synthetic/hf:nvidia/Kimi-K2.5-NVFP4".to_string(), 2);
        supervisor.update_session_limits(limits, 1).await.unwrap();

        for task_id in ["task-1", "task-2", "task-3", "task-4"] {
            supervisor
                .dispatch(task_id, project_path, "test/mock")
                .await
                .unwrap();
        }

        let err = supervisor
            .dispatch("task-5", project_path, "test/mock")
            .await
            .unwrap_err();
        assert!(matches!(err, SupervisorError::ModelAtCapacity { .. }));

        let status = supervisor.get_status().await.unwrap();
        let mock = status.capacity.get("test/mock").unwrap();
        assert_eq!(mock.max, 4);
        assert_eq!(mock.active, 4);

        let kimi = status
            .capacity
            .get("synthetic/hf:nvidia/Kimi-K2.5-NVFP4")
            .unwrap();
        assert_eq!(kimi.max, 2);
        assert_eq!(kimi.active, 0);
    }

    #[tokio::test]
    async fn interrupt_all_cancels_active_mock_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let supervisor = spawn_supervisor(&temp);
        let project_path = temp.path().to_str().unwrap();

        supervisor
            .dispatch("task-1", project_path, "test/mock")
            .await
            .unwrap();
        assert!(supervisor.has_session("task-1").await.unwrap());

        supervisor
            .interrupt_all("session interrupted by test")
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        assert!(!supervisor.has_session("task-1").await.unwrap());
    }

    #[tokio::test]
    async fn sqlite_fallback_reads_accumulated_token_counts() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("sessions.db");

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&db_path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, input_tokens INTEGER, output_tokens INTEGER, accumulated_input_tokens INTEGER, accumulated_output_tokens INTEGER)",
        )
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO sessions (id, input_tokens, output_tokens, accumulated_input_tokens, accumulated_output_tokens) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind("session-123")
        .bind(3_i64)
        .bind(5_i64)
        .bind(13_i64)
        .bind(21_i64)
        .execute(&pool)
        .await
        .unwrap();

        let tokens = AgentSupervisor::tokens_from_goose_sqlite_at(
            PathBuf::from(&db_path).as_path(),
            "session-123",
        )
        .await;

        assert_eq!(tokens, Some((13, 21)));
    }
}
