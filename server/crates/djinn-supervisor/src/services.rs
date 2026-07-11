//! The `SupervisorServices` trait — the object-safe surface the supervisor
//! orchestration loop calls into.
//!
//! See the crate docs for the PR 3 context. Two impls exist today:
//!
//! - `djinn_agent::direct_services::DirectServices` — in-process, wraps
//!   `AgentContext`. Production path.
//! - [`rpc::StubRpcServices`] — `unimplemented!()` placeholder that pins the
//!   trait layout ahead of PR 4/5.

use async_trait::async_trait;
use djinn_core::models::{SessionRecord, Task, TaskRunStatus};
use djinn_stack::environment::EnvironmentConfig;
use djinn_workspace::Workspace;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::{RoleKind, StageError, StageOutcome, TaskRunOutcome, TaskRunSpec};

pub mod rpc;
pub mod server;
pub mod wire;

pub use wire::{
    BillingSource, CostBasisHint, SerializableCreateSessionParams, SerializableCreateTaskRunParams,
    SerializableDjinnEvent,
};

/// Outcome of pushing a task branch to GitHub for a task with an existing
/// open PR.  Carries the pushed SHA on success; structured failure details
/// (mirror head, attempted GitHub head, PR-branch existence, error class)
/// on failure so callers can record structured publication-failure activity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BranchPublicationResult {
    pub success: bool,
    /// The GitHub head SHA after a successful push.
    pub pushed_sha: Option<String>,
    /// The mirror head SHA at the time of the attempt (always populated).
    pub mirror_head: String,
    /// The SHA we attempted to push to GitHub (always populated).
    pub attempted_github_head: String,
    /// Whether the PR branch already existed on GitHub before our push.
    pub pr_branch_existed: bool,
    /// Machine-readable error class on failure (e.g. "push_rejected", "auth").
    pub error_class: Option<String>,
    /// Human-readable error string on failure.
    pub error_message: Option<String>,
}

/// Dependencies shared across every stage in a task-run.
///
/// Object-safe by construction: no generic method parameters, no
/// `Self`-by-value receivers. `async_trait` handles the `Pin<Box<dyn
/// Future + Send>>` boxing so the trait can be used behind
/// `Arc<dyn SupervisorServices>`.
#[allow(clippy::too_many_arguments)]
#[async_trait]
pub trait SupervisorServices: Send + Sync + 'static {
    /// Supervisor-wide cancellation token.  Flagged when the task-run is torn
    /// down (server shutdown, user kill).
    fn cancel(&self) -> &CancellationToken;

    /// Load the [`Task`] row backing this task-run.  Called once, before the
    /// first stage executes.
    async fn load_task(&self, task_id: String) -> Result<Task, String>;

    /// Execute one role stage against the shared workspace.  Called once per
    /// entry in `spec.flow.role_sequence()`.
    async fn execute_stage(
        &self,
        task: &Task,
        workspace: &Workspace,
        role_kind: RoleKind,
        task_run_id: &str,
        spec: &TaskRunSpec,
    ) -> Result<StageOutcome, StageError>;

    /// Open (or adopt) a GitHub PR for the completed task-run.  Called at
    /// most once per run, only for `NewTask` / `ReviewResponse` /
    /// `ConflictRetry` flows that reached the end of their role sequence
    /// cleanly.
    async fn open_pr(&self, spec: &TaskRunSpec, task: &Task) -> TaskRunOutcome;

    /// Persist a new `task_run` row.  Implemented in Phase 4 (commit
    /// `a6bd7e1a4`); called by [`crate::TaskRunSupervisor::run`] so the
    /// worker pod (which has no DB connection) can ship the write through
    /// the existing `SupervisorServices` channel.
    async fn create_task_run(&self, params: SerializableCreateTaskRunParams) -> Result<(), String>;

    /// Update the terminal `status` (and `ended_at`) of a `task_run` row.
    /// Implemented in Phase 4 (commit `a6bd7e1a4`); replaces the
    /// supervisor's direct `task_runs.update_status` call.
    async fn update_task_run_status(
        &self,
        run_id: String,
        status: TaskRunStatus,
    ) -> Result<(), String>;

    /// Look up a model's context window (in tokens) from the provider
    /// catalog by full `"providerID/modelID"` identifier.
    ///
    /// Phase 6b extraction — replaces direct
    /// `agent_context.catalog.find_model(..)` reads in
    /// `supervisor_impl::stage` and `direct_services`. Returns
    /// `Err("model not found")` when the catalog has no matching entry;
    /// callers may treat that as a soft fallback to `0`.
    async fn get_model_context_window(&self, model_id: String) -> Result<i64, String>;

    /// Look up a provider's `base_url` from the provider catalog by
    /// catalog provider id.
    ///
    /// Phase 6b extraction — replaces direct
    /// `agent_context.catalog.list_providers()...find(..)` reads in
    /// `supervisor_impl::stage` and `direct_services`. Returns
    /// `Err("provider not found")` (or `Err("provider has empty base_url")`)
    /// when the catalog has no matching entry; callers may treat that as a
    /// signal to fall back to `actors::slot::helpers::default_base_url`.
    async fn get_provider_base_url(&self, catalog_provider_id: String) -> Result<String, String>;

    /// Pick any available `"providerID/modelID"` from the catalog as a
    /// fallback default model.
    ///
    /// Phase 6b extraction — replaces the legacy `default_model_for_role`
    /// helper which walked `app_state.catalog.list_providers()` /
    /// `list_models(..)` directly. Returns `Ok(None)` when the catalog has
    /// no providers / no models.
    async fn pick_any_default_model(&self) -> Result<Option<String>, String>;

    /// Create a new `session` row linked to the given task-run and emit
    /// the `session.started` SSE event.
    ///
    /// Phase 6c extraction — replaces direct
    /// `SessionRepository::new(agent_context.db, agent_context.event_bus).create(..)`
    /// in `supervisor_impl::stage`. Worker-side stubs round-trip this so the
    /// in-Pod supervisor never opens its own DB connection.
    async fn create_session(
        &self,
        params: SerializableCreateSessionParams,
    ) -> Result<SessionRecord, String>;

    /// Publish a `session.message` SSE event for the given session.
    ///
    /// Phase 6c extraction — replaces direct
    /// `agent_context.event_bus.send(DjinnEventEnvelope::session_message(..))`
    /// in `actors::slot::reply_loop`. The publish is fire-and-forget on the
    /// host (the event bus has its own back-pressure handling); the RPC
    /// returns `Ok(())` once the host has accepted the event.
    async fn publish_session_message(
        &self,
        session_id: String,
        task_id: String,
        agent_type: String,
        message: serde_json::Value,
    ) -> Result<(), String>;

    /// Fetch the project's `environment_config` blob (lifecycle hooks,
    /// language toolchains).
    ///
    /// Phase 6d extraction — replaces direct
    /// `environment::environment_config_for_project_id(&agent_context.db, ..)`
    /// in `supervisor_impl::stage`. Returns `EnvironmentConfig::empty()`
    /// (wrapped in `Ok`) for missing-project / parse-failure paths to match
    /// the existing helper's degrade-to-empty semantics.
    async fn get_environment_config(&self, project_id: String)
    -> Result<EnvironmentConfig, String>;

    /// Invoke an LLM provider once, host-side, and return the terminal
    /// aggregate of its stream as an [`djinn_provider::provider::LlmResponse`].
    ///
    /// Phase 6a-redux — the host keeps vault keys and constructs the provider
    /// from the catalog row; the worker (Phase 7) calls this method instead
    /// of `provider.stream(..)` so it never holds the API key. The reply-loop
    /// call site in `actors::slot::reply_loop` still calls
    /// `provider.stream(..)` directly today; the RPC method is dead code
    /// until Phase 7 wires the worker side.
    async fn invoke_llm(
        &self,
        model_id: String,
        conversation: djinn_provider::message::Conversation,
        tools: Vec<serde_json::Value>,
        tool_choice: Option<djinn_provider::provider::ToolChoice>,
    ) -> Result<djinn_provider::provider::LlmResponse, String>;

    /// Update an existing `session` row's status + token counts and re-emit
    /// its `session` SSE event.
    ///
    /// Phase 6e extraction — replaces the two `SessionRepository::update(..)`
    /// call sites in `supervisor_impl::stage` (the only remaining direct
    /// `SessionRepository` use there). Worker-side stubs round-trip this so
    /// the in-Pod supervisor never opens its own DB connection. Caller
    /// doesn't need the returned `SessionRecord`, so we return `()` and let
    /// the SSE side-effect carry the update outward.
    async fn update_session_status(
        &self,
        session_id: String,
        status: djinn_core::models::SessionStatus,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
        parked_reason: Option<String>,
    ) -> Result<(), String>;

    /// Best-effort mid-flight flush of a running session's cumulative token
    /// counters to the session row, so long sessions don't sit at
    /// `tokens_in = 0` in the DB (and every list/show surface reading it)
    /// until teardown. The repository guards with `status = 'running'` so a
    /// flush can never clobber a terminal row.
    ///
    /// Default is a no-op `Ok(())` so test doubles stay untouched; the two
    /// real impls (`DirectServices` host-side, `WorkerSupervisorServices`
    /// over RPC) MUST override.
    async fn flush_session_tokens(
        &self,
        _session_id: String,
        _tokens_in: i64,
        _tokens_out: i64,
        _cache_read: i64,
        _cache_write: i64,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Run the `github_search` chat-extension tool host-side and return its
    /// JSON result.
    ///
    /// Phase 7-followup gap-3 — workers have no GitHub App credentials
    /// mounted, so any in-Pod reply-loop that calls `github_search` would
    /// previously fail. `WorkerSupervisorServices` routes this over the
    /// existing RPC connection; `DirectServices` runs it locally against
    /// the host's `GitHubApiClient`. Same code path on host + worker.
    async fn tool_github_search(
        &self,
        project_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String>;

    /// Run the `github_fetch_file` chat-extension tool host-side. See
    /// [`Self::tool_github_search`] for the routing rationale.
    async fn tool_github_fetch_file(
        &self,
        project_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String>;

    /// Run the `ci_job_log` chat-extension tool host-side. Reads activity
    /// rows for the given session task id, resolves the installation token,
    /// fetches + cleans the GitHub Actions job log. See
    /// [`Self::tool_github_search`] for the routing rationale.
    async fn tool_ci_job_log(
        &self,
        session_task_id: Option<String>,
        arguments: serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value, String>;

    /// Forward a worker-emitted [`SerializableDjinnEvent`] to the host's
    /// broadcast bus so SSE subscribers (web UI live-feed) see it in real
    /// time.
    ///
    /// Phase 7-followup gap-2 — the worker's local `event_bus` is
    /// `EventBus::noop()`, so every `event_bus.send(..)` call in
    /// `actors::slot::reply_loop` / `streaming` (session_message,
    /// session_token_update) would otherwise vanish on the worker side.
    /// `WorkerSupervisorServices` delegates this over the existing TCP
    /// connection; `DirectServices` reconstructs a canonical
    /// `DjinnEventEnvelope` (interning the known `(entity_type, action)`
    /// pair to static-str) and sends it on the host's `event_bus`. The
    /// publish is fire-and-forget — the RPC reply is `Ok(())` once the
    /// host has accepted the event.
    async fn emit_djinn_event(&self, event: SerializableDjinnEvent) -> Result<(), String>;

    /// Touch the host's ActivityTracker for the given task_id so the
    /// coordinator's stall poller (enforce_session_stall_timeout, 5-min
    /// worker / 10-min architect threshold) doesn't reap a long-running
    /// worker mid-flow. Worker calls this from reply_loop on every LLM
    /// turn + tool call. Host-side DirectServices forwards to
    /// agent_context.touch_activity(task_id).
    ///
    /// Fire-and-forget shape — never returns Err (any RPC transport
    /// failure is silently logged on the host side; we'd rather risk
    /// the false-stall than fail a worker on transient flakes).
    ///
    /// Phase 7-followup BLOCKER for the smoke test: K8s workers
    /// construct a fresh empty ActivityTracker at startup
    /// (agent-worker main.rs build of AgentContext), so calls to
    /// `app_state.register_activity(task_id)` /
    /// `activity_ts.store(..)` in `actors::slot::reply_loop` only
    /// touch the WORKER's local tracker. The host's coordinator
    /// stall poller reads `app_state.idle_seconds(task_id)` from its
    /// OWN tracker — sees `None` — falls back to wall-clock since
    /// `session.started_at` — and kills every worker session at the
    /// 5-minute mark, mid-LLM-stream. This RPC bridges the two
    /// trackers so the host's poller stays accurate.
    async fn touch_activity(&self, task_id: String) -> Result<(), String>;

    /// Emit a coarse stage-init progress marker (see
    /// [`djinn_runtime::StreamEvent::StageStep`]) so the host-side pre-session
    /// liveness deadline can name the in-pod step a hung setup is stuck on
    /// (workspace attach, cargo seed, context build, ...) and detect when the
    /// first reply-loop turn is reached ([`djinn_runtime::STAGE_STEP_FIRST_TURN`]).
    ///
    /// Fire-and-forget and diagnostic-only: the default impl is a no-op, so
    /// only the in-pod worker transport (which forwards it on the shared frame
    /// channel as a [`djinn_runtime::WorkerEvent::StageStep`]) needs to
    /// override it. Host-side / test service impls that drive the run in-process
    /// leave it a no-op — the deadline's DB-truth backstop covers those paths.
    async fn report_stage_step(&self, _step: &'static str) -> Result<(), String> {
        Ok(())
    }

    /// Apply a [`djinn_core::models::TransitionAction`] to the given task
    /// on the host. The supervisor stage loop calls this at every stage
    /// boundary so the task walks its full status machine
    /// (`Start` → `SubmitTaskReview` → `TaskReviewStart` →
    /// `TaskReviewApprove` / `TaskReviewReject` → `PrCreated`) even on
    /// the K8s path where the supervisor body has no direct DB access.
    ///
    /// `action` is the wire-string form parsed by
    /// `TransitionAction::parse`. `reason` is required only for actions
    /// whose `requires_reason()` returns true (the host enforces this
    /// and surfaces `InvalidTransition` if missing).
    ///
    /// Failures arrive as `Err(String)`:
    /// - parse failures (`"unknown transition action: ..."`),
    /// - state-machine validation (`"... is only valid from ..."`),
    /// - DB / transport errors.
    ///
    /// Callers SHOULD treat `InvalidTransition` as a soft skip
    /// (idempotency: a re-dispatched task may already be in the target
    /// state) and log-only on transport failures rather than failing
    /// the run.
    async fn transition_task(
        &self,
        task_id: String,
        action: String,
        reason: Option<String>,
    ) -> Result<(), String>;

    /// Persist an arbiter decision (approve or approve_conflict) on the
    /// current unconsumed arbitration row and emit an `arbiter_decision`
    /// activity event.
    ///
    /// Called after the pre-approval gate passes and before the board
    /// transition so the arbitration row carries the decision payload
    /// and evidence (AC2).  Failures are non-fatal — callers log and
    /// proceed with the transition.
    async fn record_arbiter_decision(
        &self,
        task_id: String,
        decision: String,
        evidence_json: String,
    ) -> Result<(), String>;

    /// Start a monitored-reopen worker attempt for an arbiter `reopen`
    /// decision.  Persists the directive, verification command, and
    /// excluded models on the current arbitration row, then atomically
    /// marks the attempt start via `record_monitored_reopen` so re-entry
    /// cannot inject the directive twice.
    ///
    /// Emits an `arbiter_decision` activity event with the reopen
    /// decision.  Failures are non-fatal — callers log and proceed with
    /// the `lead_intervention_complete` transition that returns the task
    /// to `open` for a fresh worker dispatch.
    async fn start_monitored_reopen(
        &self,
        task_id: String,
        directive: String,
        verification_command: String,
        exclude_models: Vec<String>,
    ) -> Result<(), String>;

    /// Mark the monitored-reopen attempt as complete.  Called on any terminal
    /// outcome of the monitored worker attempt — worker submit, reviewer
    /// rejection, CI failure, worker failure, or no-eligible-model.
    /// Transitions the arbitration row to `consumed` (terminal for this hold
    /// cycle) so re-entry cannot trigger a second arbiter or worker retry.
    /// Failures are non-fatal — callers log and continue.
    async fn complete_monitored_reopen(&self, task_id: String) -> Result<(), String>;

    /// Record the termination of an arbiter session for bounded accounting.
    ///
    /// `is_infra_failure` is `true` when the failure was a provider/infra
    /// error (before any decision was made), `false` when the session ran
    /// but ended without a valid decision.
    ///
    /// Returns `Ok(true)` when the decision-failure cap was reached and the
    /// arbitration was parked with a generated failure dossier (no further
    /// arbiter dispatch for this hold cycle), `Ok(false)` when the
    /// termination was recorded but the arbiter may be re-dispatched.
    ///
    /// Infra failures after a decision has been accepted (consumed
    /// arbitration) are no-ops — they do not mutate accounting.
    async fn record_arbiter_session_termination(
        &self,
        task_id: String,
        is_infra_failure: bool,
    ) -> Result<bool, String>;

    /// Push the task branch to GitHub for a task with an existing open PR,
    /// so GitHub Actions evaluates the worker's latest mirror commit.
    ///
    /// Called after a successful WorkerDone mirror push.  The host-side
    /// `DirectServices` impl resolves GitHub coords, mints an installation
    /// token, and delegates to the existing `push_task_branch_to_github`
    /// helper (reusing its concurrent-push race guard).  The worker-side
    /// RPC stub forwards over the wire.
    ///
    /// Default is a no-op success so test doubles stay untouched; real
    /// impls (`DirectServices`, `RpcServices`) MUST override.
    async fn publish_branch_to_github(
        &self,
        _spec: &TaskRunSpec,
        _task: &Task,
    ) -> BranchPublicationResult {
        BranchPublicationResult {
            success: true,
            pushed_sha: None,
            mirror_head: String::new(),
            attempted_github_head: String::new(),
            pr_branch_existed: false,
            error_class: None,
            error_message: None,
        }
    }
}
