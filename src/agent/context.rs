use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::actors::coordinator::VerificationTracker;
use crate::agent::file_time::FileTime;
use crate::agent::lsp::LspManager;
use crate::agent::roles::RoleRegistry;
use crate::db::connection::Database;
use crate::events::EventBus;
use crate::provider::HealthTracker;
use djinn_git::GitActorHandle;

/// Subset of application state required by agent lifecycle, coordinator, and
/// slot code.  Cheaply cloneable — all fields are either `Clone` or wrapped in
/// `Arc`.
///
/// Construct via [`AppState::agent_context()`].
#[derive(Clone)]
pub struct AgentContext {
    pub db: Database,
    pub event_bus: EventBus,
    pub git_actors: Arc<Mutex<HashMap<PathBuf, GitActorHandle>>>,
    pub verifying_tasks: VerificationTracker,
    pub role_registry: Arc<RoleRegistry>,
    pub health_tracker: HealthTracker,
    pub file_time: Arc<FileTime>,
    pub lsp: LspManager,
}
