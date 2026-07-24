// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
//! Bincode wire envelope for [`SupervisorServices`].
//!
//! Phase 2 K8s PR 2 of `/home/fernando/.claude/plans/phase2-k8s-scaffolding.md`.
//! The production transport is TCP (worker Pods dialling the djinn-server
//! ClusterIP Service); the original unix-socket transport still exists on
//! the launcher side for in-process tests.
//!
//! This module lives inside `djinn-supervisor` (rather than upstream in
//! `djinn-runtime`) because the request/response variants reference
//! supervisor-owned types ([`Task`], [`TaskRunSpec`], [`StageOutcome`],
//! [`StageError`], [`TaskRunOutcome`]).  Pushing the envelope here avoids a
//! circular dep with `djinn-runtime`: the runtime crate owns the transport
//! primitives (`WorkspaceRef`, `Frame` header bytes, codec helpers — see
//! `djinn_runtime::wire`); this module owns the *contents* a `SupervisorServices`
//! peer would ship.
//!
//! # Layout
//!
//! * [`Frame`] — correlation-id + [`FramePayload`] pair.  `correlation_id` is
//!   a monotonically-increasing `u64` allocated by the worker for each RPC;
//!   the launcher echoes the same value on the matching reply.
//! * [`FramePayload`] — variant-select between `Rpc`, `RpcReply`, `Event`
//!   (placeholder upstream for PR 6+), and `Control` (`Cancel` / `Shutdown`).
//! * [`ServiceRpcRequest`] / [`ServiceRpcResponse`] — one variant per
//!   [`SupervisorServices`] trait method.
//!
//! # Wire framing
//!
//! `Frame` values are written via `djinn_runtime::wire::write_frame` — a
//! `u32` big-endian length header followed by the bincode body.  The codec
//! helpers live in `djinn-runtime::wire` so both the launcher server side and
//! the worker client side use the same reader/writer pair.

use djinn_core::models::{SessionRecord, SessionStatus, Task, TaskRunStatus};
use djinn_runtime::wire::{ControlMsg, WorkerEvent, WorkspaceRef};
use serde::{Deserialize, Serialize};

use crate::services::lease::{
    LeaseAbandonRequest, LeaseBindRequest, LeaseCancelRequest, LeaseGrantRequest,
    LeaseQueueRequest, LeaseReleaseRequest, LeaseResult, LeaseStatusRequest,
    WatchdogTerminationRequest,
};
use crate::{
    BranchPublicationResult, RoleKind, StageError, StageOutcome, TaskRunOutcome, TaskRunSpec,
};

/// Top-level wire envelope.
///
/// Every byte sent in either direction is a length-prefixed bincode-serialized
/// `Frame`.  `correlation_id` is meaningful only for the [`FramePayload::Rpc`]
/// ↔ [`FramePayload::RpcReply`] round-trip — `Event` and `Control` payloads
/// carry a placeholder `0` (ignored by both sides).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame {
    pub correlation_id: u64,
    pub payload: FramePayload,
}

/// Multiplexed payload carried on the duplex frame channel.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum FramePayload {
    /// Worker → launcher request.
    Rpc(ServiceRpcRequest),
    /// Launcher → worker reply, matched to the originating request via
    /// [`Frame::correlation_id`].
    RpcReply(ServiceRpcResponse),
    /// Worker → launcher upstream event (PR 5 has no producers; the variant
    /// exists so the wire shape is stable when PR 6+ starts emitting).
    Event(WorkerEvent),
    /// Launcher → worker control signal — cancel / shutdown.  Travels
    /// out-of-band of the request/reply correlation.
    Control(ControlMsg),
    /// Worker → launcher handshake.  MUST be the first frame on a TCP
    /// connection (Phase 2 K8s PR 2).  The launcher validates the bearer
    /// token via an injected [`crate::services::server::TokenValidator`]
    /// before accepting any subsequent [`FramePayload::Rpc`].  Not used on
    /// the in-process unix-socket test path (that path trusts the
    /// filesystem permissions on the socket).
    AuthHello(AuthHelloMsg),
    /// Launcher → worker auth result, delivered in response to an
    /// [`FramePayload::AuthHello`] on the TCP path.  On rejection the
    /// launcher writes this frame then closes the connection.
    AuthResult(AuthResultMsg),
}

/// Contents of an [`FramePayload::AuthHello`] handshake frame.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthHelloMsg {
    /// The task-run id the worker was launched to serve.  Used to
    /// demultiplex connections on the single TCP listener.
    pub task_run_id: String,
    /// Bearer token the worker read from its projected ServiceAccount
    /// token file (or any other equivalent source).  The server validates
    /// this via the injected [`crate::services::server::TokenValidator`].
    pub token: String,
}

/// Borrow-free serde-friendly twin of
/// [`djinn_db::repositories::task_run::CreateTaskRunParams<'_>`].
///
/// The repo struct stores `&str` fields so SQL parameter binding stays
/// zero-copy; sending it across the bincode wire requires owned `String`s.
/// The shapes line up 1:1 — adapt by `.as_str()` on the host before
/// constructing the repo params.  Introduced in Phase 3 of
/// `~/.claude/plans/phase2-worker-execution-architecture.md`; dead code
/// until Phase 4 wires the supervisor's
/// [`crate::TaskRunSupervisor::run`] body off
/// [`djinn_db::TaskRunRepository`] and onto
/// [`crate::SupervisorServices::create_task_run`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SerializableCreateTaskRunParams {
    pub id: String,
    pub task_attempt_id: Option<String>,
    pub project_id: String,
    pub task_id: String,
    pub trigger_type: String,
    /// Initial status; `None` defaults to `"running"` on the host side.
    pub status: Option<String>,
    pub workspace_path: Option<String>,
    pub mirror_ref: Option<String>,
    /// Exact coordinator process identity for this dispatch. Optional during mixed-version rollout.
    pub dispatch_owner_incarnation_id: Option<String>,
    /// One coordinator-minted correlation group for this whole dispatch.
    pub dispatch_group_id: Option<String>,
}

/// Serde-friendly twin of [`djinn_core::events::DjinnEventEnvelope`].
///
/// The canonical envelope stores `entity_type` and `action` as
/// `&'static str` because every constructor on the host side is a
/// statically-known event family — that lets host-side actors pattern-match
/// on the pair without allocating. The worker → host RPC path doesn't have
/// that luxury (the wire deserialises into owned `String`s), so this wire
/// twin is what crosses the TCP frame and the host's
/// `DirectServices::emit_djinn_event` interns the strings back into the
/// known static-str pair before forwarding to the broadcast bus.
///
/// `from_sync` is intentionally omitted: the host emits the value with
/// `from_sync = false` (the worker is not the sync channel), matching the
/// `#[serde(skip)]` on the canonical struct.
///
/// `payload` is shipped as an opaque JSON `String` rather than
/// `serde_json::Value` because bincode is a positional codec that rejects
/// `serde_json::Value`'s untagged-enum representation with
/// `"does not support deserialize_any"`. The host re-parses it via
/// `serde_json::from_str` in `DirectServices::emit_djinn_event`. Same logic
/// drove the `OAuthConfigWire` pattern in `djinn-agent`'s slot helpers.
///
/// `id` / `project_id` keep the `Option<String>` type but intentionally do
/// NOT carry `#[serde(skip_serializing_if = "Option::is_none")]` — that
/// attribute is JSON-only. Bincode encodes `Option` as a single discriminant
/// byte followed by the inner payload when `Some`; if the serializer skips
/// the field, the deserializer reads garbage from the next field's slot.
/// Every envelope with both Options unset (e.g. `activity.logged`)
/// silently corrupted on the wire before this fix.
///
/// Introduced in the Phase 7-followup gap-2 wiring so worker-side
/// `event_bus.send(..)` calls (in `reply_loop` / `streaming`) reach the
/// host's SSE subscribers instead of vanishing into the worker's noop bus.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SerializableDjinnEvent {
    pub entity_type: String,
    pub action: String,
    /// JSON-serialised `serde_json::Value`. Opaque on the wire; the host
    /// re-parses via `serde_json::from_str` before forwarding to the
    /// broadcast bus.
    pub payload: String,
    pub id: Option<String>,
    pub project_id: Option<String>,
}

impl SerializableDjinnEvent {
    /// Lossy snapshot of a [`djinn_core::events::DjinnEventEnvelope`] —
    /// drops `from_sync` (the wire path is implicitly `from_sync = false`)
    /// and serialises `payload` to an opaque JSON string for bincode safety.
    pub fn from_envelope(envelope: &djinn_core::events::DjinnEventEnvelope) -> Self {
        Self {
            entity_type: envelope.entity_type.to_string(),
            action: envelope.action.to_string(),
            payload: serde_json::to_string(&envelope.payload).unwrap_or_default(),
            id: envelope.id.clone(),
            project_id: envelope.project_id.clone(),
        }
    }
}

/// Borrow-free serde-friendly twin of
/// [`djinn_db::repositories::session::CreateSessionParams<'_>`].
///
/// Phase 6c extraction (per
/// `~/.claude/plans/phase2-worker-execution-architecture.md`). The repo
/// struct stores `&str` fields so SQL parameter binding stays zero-copy;
/// sending it across the bincode wire requires owned `String`s. The shapes
/// line up 1:1 — adapt by `.as_str()` / `.as_deref()` on the host before
/// constructing the repo params.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SerializableCreateSessionParams {
    pub project_id: String,
    pub task_id: Option<String>,
    pub model: String,
    pub agent_type: String,
    pub metadata_json: Option<String>,
    pub task_run_id: Option<String>,
    /// Explicit billing classification derived from the resolved credential +
    /// catalog/provider context at model-resolution time. When present,
    /// [`DirectServices::create_session`] uses this as the primary signal for
    /// `sessions.cost_basis` instead of falling back to
    /// `classify_provider(provider_id)`.
    #[serde(default)]
    pub cost_basis_hint: Option<CostBasisHint>,
    /// Kind of credential backing the session, derived from the resolved
    /// credential at model-resolution time. Persisted to `sessions.billing_source`
    /// by the host so plan-vs-API-key usage is queryable after the fact. `None`
    /// for callers with no dispatch-time credential signal (the column stays
    /// `NULL`).
    #[serde(default)]
    pub billing_source: Option<BillingSource>,
}

/// Kind of credential backing a session, for `sessions.billing_source`.
///
/// Distinct from [`CostBasisHint`]: the hint decides how to interpret
/// `cost_usd` (projected vs actual), while this records the concrete credential
/// kind so the OAuth-plan case — which is invisible in the `model_id` string
/// (e.g. `openai/gpt-5.5` backed by a ChatGPT/Codex plan) — is auditable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingSource {
    /// A personal subscription-plan OAuth credential (ChatGPT/Codex plan,
    /// GitHub Copilot). Real per-token API spend is $0.
    PlanOauth,
    /// An API key — metered pay-as-you-go, or a coding-plan API key (whose plan
    /// nature is already captured by `cost_basis = 'projected'`).
    ApiKey,
}

impl BillingSource {
    /// Database text representation for `sessions.billing_source` (matches the
    /// migration 88 CHECK constraint).
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::PlanOauth => "plan_oauth",
            Self::ApiKey => "api_key",
        }
    }
}

/// Billing classification hint derived from the resolved credential and
/// provider catalog context.
///
/// Passed through [`SerializableCreateSessionParams`] so the host-side
/// `DirectServices::create_session` can choose `sessions.cost_basis` by
/// precedence:
///
/// 1. Explicit subscription hint → `"projected"`
/// 2. Explicit metered hint → priced `"actual"` / unpriced `"unpriced"`
/// 3. Unknown → fall back to `governable_subscription_for_model` /
///    `classify_provider` + pricing availability
///
/// This replaces the previous single-level `classify_provider` check which
/// missed Codex/OAuth credentials surfacing under the `openai` namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasisHint {
    /// The credential is a personal subscription plan (e.g. Codex OAuth,
    /// coding-plan provider). Sessions should use `"projected"` cost basis.
    SubscriptionPlan,
    /// The credential is a metered API key with standard per-token billing.
    /// Sessions should use `"actual"` when pricing exists, `"unpriced"` when
    /// not.
    MeteredApi,
}

/// Contents of an [`FramePayload::AuthResult`] handshake reply.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthResultMsg {
    /// Whether the token was accepted.
    pub accepted: bool,
    /// Optional human-readable reason surfaced when `accepted == false`.
    pub error: Option<String>,
}

/// Attributed request for a dedicated host-side planner LLM call.
///
/// Carries explicit project/task/task-run/session/creator attribution so the
/// host can durably record the attempt before provider I/O and finalize it with
/// the actual usage, catalog price snapshots, and terminal outcome. The
/// `conversation` and `tools` fields are JSON-encoded strings so the wire
/// remains compatible with the positional bincode codec.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttributedPlannerRequest {
    pub project_id: String,
    pub task_id: String,
    /// Required real task-run identity. This operation deliberately has no
    /// anonymous or unattributed mode.
    pub task_run_id: String,
    /// Required real role-session identity.
    pub session_id: String,
    /// Required creator identity used for caller-scoped credential resolution.
    pub created_by_user_id: String,
    pub operation: String,
    pub prompt_id: String,
    pub conversation: String,
    pub tools: String,
    pub tool_choice: Option<djinn_provider::provider::ToolChoice>,
    pub max_tokens: u32,
    pub timeout_ms: u64,
}

/// Terminal outcome of a dedicated host-side planner LLM call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlannerOutcome {
    Success,
    Timeout,
    InvalidPayload,
    ProviderError,
}

/// Result of a dedicated host-side planner LLM call.
///
/// On success, `content` holds the raw completion text. On failure, `content`
/// is `None` and `diagnostic` carries a bounded description. The four usage
/// counters and `cost_usd` are always the latest values observed by the host
/// before terminal finalization.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannerAttemptResult {
    pub outcome: PlannerOutcome,
    pub content: Option<String>,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_usd: Option<f64>,
    pub diagnostic: Option<String>,
}

/// Typed request variants — one per trait method on [`crate::SupervisorServices`]
/// except `cancel()`, which is satisfied locally on the worker and does not
/// cross the wire.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum ServiceRpcRequest {
    /// [`crate::SupervisorServices::load_task`].
    LoadTask { task_id: String },
    /// [`crate::SupervisorServices::execute_stage`].  `workspace` is shipped
    /// as a [`WorkspaceRef`] — the launcher rehydrates it via
    /// `Workspace::attach_existing` before delegating to the concrete impl.
    ExecuteStage {
        task: Task,
        workspace: WorkspaceRef,
        role_kind: RoleKind,
        task_run_id: String,
        spec: TaskRunSpec,
    },
    /// [`crate::SupervisorServices::open_pr`].
    OpenPr { spec: TaskRunSpec, task: Task },
    /// [`crate::SupervisorServices::create_task_run`].  Wire surface added
    /// in Phase 3; driven by `TaskRunSupervisor::run` as of Phase 4 (commit
    /// `a6bd7e1a4`).
    CreateTaskRun {
        params: SerializableCreateTaskRunParams,
    },
    /// [`crate::SupervisorServices::update_task_run_status`].
    UpdateTaskRunStatus {
        run_id: String,
        status: TaskRunStatus,
    },
    /// [`crate::SupervisorServices::get_model_context_window`].
    /// Phase 6b — catalog read extraction.
    GetModelContextWindow { model_id: String },
    /// [`crate::SupervisorServices::get_provider_base_url`].
    /// Phase 6b — catalog read extraction.
    GetProviderBaseUrl { catalog_provider_id: String },
    /// [`crate::SupervisorServices::pick_any_default_model`].
    /// Phase 6b — catalog read extraction.
    PickAnyDefaultModel,
    /// [`crate::SupervisorServices::create_session`].  Phase 6c — session
    /// persistence extraction.
    CreateSession {
        params: SerializableCreateSessionParams,
    },
    /// [`crate::SupervisorServices::publish_session_message`].  Phase 6c —
    /// session persistence extraction.
    ///
    /// `message` is shipped as an opaque JSON `String` rather than a
    /// `serde_json::Value` because bincode is a positional codec that
    /// rejects `serde_json::Value`'s untagged-enum internals with
    /// `DeserializeAnyNotSupported`. The host re-parses via
    /// `serde_json::from_str` in `DirectServices::publish_session_message`.
    /// Same logic drove the `SerializableDjinnEvent.payload` shape.
    PublishSessionMessage {
        session_id: String,
        task_id: String,
        agent_type: String,
        message: String,
    },
    /// [`crate::SupervisorServices::get_environment_config`].  Phase 6d —
    /// project environment-config extraction.
    GetEnvironmentConfig { project_id: String },
    /// [`crate::SupervisorServices::invoke_llm`].  Phase 6a-redux —
    /// host-side LLM invocation. The worker (Phase 7) will use this to keep
    /// vault credentials off the worker side.
    ///
    /// `conversation` + `tools` are shipped as opaque JSON `String`s for
    /// bincode safety: both pull in `serde_json::Value` (directly in
    /// `tools`; transitively via `ContentBlock::ToolUse.input` and
    /// `MessageMeta.provider_data` inside the conversation) and
    /// `ContentBlock`'s internally-tagged enum representation, neither
    /// of which the positional bincode codec can `deserialize_any`.
    /// The host re-parses both via `serde_json::from_str` before
    /// invoking the trait method, which keeps the ergonomic typed
    /// surface unchanged.
    InvokeLlm {
        model_id: String,
        conversation: String,
        tools: String,
        tool_choice: Option<djinn_provider::provider::ToolChoice>,
    },
    /// [`crate::SupervisorServices::update_session_status`].  Phase 6e —
    /// finishes the session-persistence extraction started in 6c so
    /// `supervisor_impl::stage` no longer constructs a `SessionRepository`.
    UpdateSessionStatus {
        session_id: String,
        status: SessionStatus,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
        parked_reason: Option<String>,
    },
    /// [`crate::SupervisorServices::emit_djinn_event`].  Phase 7-followup
    /// gap-2 — bridges worker-side `event_bus.send(..)` calls to the host's
    /// broadcast bus so SSE subscribers (web UI session live-feed) see
    /// session-message / token-update / lifecycle events in real time.
    EmitDjinnEvent { event: SerializableDjinnEvent },
    /// [`crate::SupervisorServices::tool_github_search`]. Phase 7-followup
    /// gap-3 — workers have no GitHub App credentials mounted; this RPC
    /// runs the tool host-side.
    ///
    /// `arguments` is shipped as an opaque JSON `String` (the JSON-encoded
    /// `serde_json::Map<String, serde_json::Value>`) for bincode safety —
    /// see the comment on `PublishSessionMessage.message`.
    ToolGithubSearch {
        project_id: Option<String>,
        arguments: String,
    },
    /// [`crate::SupervisorServices::tool_github_fetch_file`]. Phase 7-followup
    /// gap-3. `arguments` is opaque JSON; see `ToolGithubSearch`.
    ToolGithubFetchFile {
        project_id: Option<String>,
        arguments: String,
    },
    /// [`crate::SupervisorServices::tool_ci_job_log`]. Phase 7-followup
    /// gap-3. `arguments` is opaque JSON; see `ToolGithubSearch`.
    ToolCiJobLog {
        session_task_id: Option<String>,
        arguments: String,
    },
    /// [`crate::SupervisorServices::touch_activity`]. Phase 7-followup
    /// BLOCKER — bridges worker-side activity-tracker touches into the
    /// host's tracker so the coordinator's stall poller doesn't reap a
    /// long-running K8s worker mid-flow.
    TouchActivity { task_id: String },
    /// [`crate::SupervisorServices::transition_task`]. Lets the in-Pod
    /// supervisor walk the task through its status machine (Start →
    /// SubmitTaskReview → TaskReviewStart → TaskReviewApprove/Reject →
    /// PrCreated) at stage boundaries. `action` is the wire-string form
    /// parsed by `TransitionAction::parse` on the host.
    TransitionTask {
        task_id: String,
        action: String,
        reason: Option<String>,
    },
    /// [`crate::SupervisorServices::flush_session_tokens`]. Mid-flight
    /// best-effort token-counter flush so long-running sessions don't show
    /// `tokens_in = 0` until teardown. Appended at the enum tail — the
    /// positional bincode codec encodes the variant index, so inserting
    /// anywhere else would shift every later variant and break
    /// mixed-version host/worker frames mid-deploy.
    FlushSessionTokens {
        session_id: String,
        tokens_in: i64,
        tokens_out: i64,
        cache_read: i64,
        cache_write: i64,
    },
    /// RESERVED (removed): was the arbiter preapproval-gate request. Kept
    /// as a fieldless placeholder to preserve positional bincode variant
    /// indices for mixed-version host/worker frames.
    #[allow(dead_code)]
    ReservedRemovedArbiterGate,
    /// [`crate::SupervisorServices::record_arbiter_decision`].
    /// Persists an arbiter decision on the arbitration row and emits
    /// an `arbiter_decision` activity event.  Appended at the enum
    /// tail for bincode stability.
    RecordArbiterDecision {
        task_id: String,
        decision: String,
        evidence_json: String,
    },
    /// [`crate::SupervisorServices::start_monitored_reopen`].
    /// Starts a monitored-reopen worker attempt by persisting the
    /// directive / verification command / excluded models and marking
    /// the attempt start.  Appended at the enum tail for bincode
    /// stability.
    StartMonitoredReopen {
        task_id: String,
        directive: String,
        verification_command: String,
        exclude_models: Vec<String>,
    },
    /// [`crate::SupervisorServices::complete_monitored_reopen`].
    /// Marks the monitored-reopen attempt complete on a terminal worker
    /// outcome.  Appended at the enum tail for bincode stability.
    CompleteMonitoredReopen { task_id: String },
    /// [`crate::SupervisorServices::record_arbiter_session_termination`].
    /// Records bounded session termination accounting.  Appended at the
    /// enum tail for bincode stability.
    RecordArbiterSessionTermination {
        task_id: String,
        is_infra_failure: bool,
    },
    /// [`crate::SupervisorServices::publish_branch_to_github`].
    /// Pushes the task branch to GitHub for a task with an existing open PR
    /// so GitHub Actions evaluates the latest mirror commit.  Appended at the
    /// enum tail for bincode stability.
    PublishBranchToGithub { spec: TaskRunSpec, task: Task },
    /// Dedicated host-side planner LLM call with durable attribution.
    /// Appended at the enum tail for bincode stability.
    PlanMemoryIntents { request: AttributedPlannerRequest },
    /// [`crate::SupervisorServices::tool_ci_artifact`]. `arguments` is opaque
    /// JSON (the JSON-encoded `serde_json::Map<String, serde_json::Value>`);
    /// see `ToolGithubSearch`. Appended at the enum tail for bincode stability.
    ToolCiArtifact {
        session_task_id: Option<String>,
        arguments: String,
    },
    /// [`crate::SupervisorServices::queue_lease`].  Lease-v1 variants are
    /// appended at the enum tail to preserve positional bincode indices for
    /// mixed-version host/worker frames mid-deploy.
    QueueLease { request: LeaseQueueRequest },
    /// [`crate::SupervisorServices::grant_lease`]. Appended at the tail.
    GrantLease { request: LeaseGrantRequest },
    /// [`crate::SupervisorServices::lease_status`]. Appended at the tail.
    LeaseStatus { request: LeaseStatusRequest },
    /// [`crate::SupervisorServices::abandon_lease`]. Appended at the tail.
    AbandonLease { request: LeaseAbandonRequest },
    /// [`crate::SupervisorServices::bind_lease_pod`]. Appended at the tail.
    BindLeasePod { request: LeaseBindRequest },
    /// [`crate::SupervisorServices::cancel_lease`]. Appended at the tail.
    CancelLease { request: LeaseCancelRequest },
    /// [`crate::SupervisorServices::release_lease`]. Appended at the tail.
    ReleaseLease { request: LeaseReleaseRequest },
    /// Exact immutable Pod watchdog termination. Appended for bincode stability.
    TerminateWatchdogPod { request: WatchdogTerminationRequest },
}

/// Typed response variants — one per [`ServiceRpcRequest`] variant.
///
/// `Err(String)` is reserved for transport-level failures (the worker
/// encountered a protocol violation / connection reset / serialization
/// error).  Semantic errors inside a call (e.g. `load_task` returning
/// `Err("task not found")`) travel inside the matching typed variant.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum ServiceRpcResponse {
    LoadTask(Result<Task, String>),
    ExecuteStage(Result<StageOutcome, StageError>),
    OpenPr(TaskRunOutcome),
    CreateTaskRun(Result<(), String>),
    UpdateTaskRunStatus(Result<(), String>),
    /// Phase 6b — catalog read responses.
    GetModelContextWindow(Result<i64, String>),
    GetProviderBaseUrl(Result<String, String>),
    PickAnyDefaultModel(Result<Option<String>, String>),
    /// Phase 6c — session persistence responses.
    CreateSession(Result<SessionRecord, String>),
    PublishSessionMessage(Result<(), String>),
    /// Phase 6d — project environment-config response.
    ///
    /// `Ok` carries the JSON-encoded `EnvironmentConfig` as an opaque
    /// string. `EnvironmentConfig` embeds `HookCommand`, which uses
    /// `#[serde(untagged)]` to keep the on-disk / Dolt JSON shape
    /// devcontainer-compatible (a hook is either a shell string, an
    /// argv array, or a named map). Bincode rejects untagged enums
    /// with `DeserializeAnyNotSupported`, so we ship the JSON
    /// representation verbatim and re-parse on receive. Disk + Dolt
    /// persistence shape stays untouched.
    GetEnvironmentConfig(Result<String, String>),
    /// Phase 6a-redux — host-side LLM invocation response.
    ///
    /// `Ok` carries the JSON-encoded `LlmResponse` as an opaque string.
    /// `LlmResponse` contains `Vec<ContentBlock>`, whose internally-tagged
    /// enum representation and `ToolUse.input: serde_json::Value` field
    /// both blow up bincode's positional codec with
    /// `DeserializeAnyNotSupported`. The host re-parses on receive.
    InvokeLlm(Result<String, String>),
    /// Phase 6e — session status update response.
    UpdateSessionStatus(Result<(), String>),
    /// Phase 7-followup gap-2 — fire-and-forget event-bridge ack.
    EmitDjinnEvent(Result<(), String>),
    /// Phase 7-followup gap-3 — host-side `github_search` tool result.
    /// `Ok` carries opaque JSON (the JSON-encoded `serde_json::Value`);
    /// see `ServiceRpcRequest::ToolGithubSearch` for the rationale.
    ToolGithubSearch(Result<String, String>),
    /// Phase 7-followup gap-3 — host-side `github_fetch_file` tool result.
    /// `Ok` carries opaque JSON; see `ToolGithubSearch`.
    ToolGithubFetchFile(Result<String, String>),
    /// Phase 7-followup gap-3 — host-side `ci_job_log` tool result.
    /// `Ok` carries opaque JSON; see `ToolGithubSearch`.
    ToolCiJobLog(Result<String, String>),
    /// Phase 7-followup BLOCKER — fire-and-forget activity-touch ack.
    TouchActivity(Result<(), String>),
    /// Supervisor-driven task-status transition ack. `Err(String)` carries
    /// the host's parse/transition error (e.g. invalid wire action, or
    /// `InvalidTransition` from the state machine).
    TransitionTask(Result<(), String>),
    /// Transport-level failure — not produced by normal operation.
    Err(String),
    /// Mid-flight token-flush ack. Appended after `Err` to keep every
    /// pre-existing variant index stable for the positional bincode codec
    /// (see `ServiceRpcRequest::FlushSessionTokens`).
    FlushSessionTokens(Result<(), String>),
    /// RESERVED (removed): was the arbiter preapproval-gate response.
    /// Placeholder preserves positional bincode index.
    #[allow(dead_code)]
    ReservedRemovedArbiterGate(Result<(), String>),
    /// Arbiter decision persistence ack.  `Err` carries the host's
    /// error.  Appended at the enum tail for bincode stability.
    RecordArbiterDecision(Result<(), String>),
    /// Monitored-reopen attempt-start ack.  `Err` carries the host's
    /// error.  Appended at the enum tail for bincode stability.
    StartMonitoredReopen(Result<(), String>),
    /// Monitored-reopen attempt-completion ack.  `Err` carries the host's
    /// error.  Appended at the enum tail for bincode stability.
    CompleteMonitoredReopen(Result<(), String>),
    /// Arbiter session termination accounting ack.  `Ok(true)` when the
    /// decision-failure cap was reached and the arbitration was parked.
    /// Appended at the enum tail for bincode stability.
    RecordArbiterSessionTermination(Result<bool, String>),
    /// Branch publication result.  `Ok` carries the publication outcome;
    /// `Err` is a transport/infra failure.  Appended at the enum tail for
    /// bincode stability.
    PublishBranchToGithub(BranchPublicationResult),
    /// Dedicated host-side planner LLM call result.  Appended at the enum
    /// tail for bincode stability.
    PlanMemoryIntents(Result<PlannerAttemptResult, String>),
    /// Phase 7-followup gap-3 — host-side `ci_artifact` tool result.
    /// `Ok` carries opaque JSON; see `ToolGithubSearch`.
    /// Appended at the enum tail for bincode stability.
    ToolCiArtifact(Result<String, String>),
    QueueLease(LeaseResult),
    GrantLease(LeaseResult),
    LeaseStatus(LeaseResult),
    AbandonLease(LeaseResult),
    BindLeasePod(LeaseResult),
    CancelLease(LeaseResult),
    ReleaseLease(LeaseResult),
    TerminateWatchdogPod(Result<(), String>),
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use djinn_core::models::TaskRunTrigger;
    use djinn_runtime::{SupervisorFlow, TaskRunSpec};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn fake_task() -> Task {
        Task {
            id: "t1".into(),
            project_id: "p1".into(),
            short_id: "T-1".into(),
            epic_id: None,
            title: "t".into(),
            description: "d".into(),
            design: "".into(),
            issue_type: "task".into(),
            status: "open".into(),
            priority: 0,
            owner: "fernando".into(),
            labels: "[]".into(),
            acceptance_criteria: "[]".into(),
            reopen_count: 0,
            continuation_count: 0,
            total_reopen_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "now".into(),
            updated_at: "now".into(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".into(),
            agent_type: None,
            created_by_user_id: "fixture-user".into(),
            ci_status: "unknown".into(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".into(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            ci_mq_state: None,
            ci_mq_run_id: None,
            ci_mq_head_sha: None,
            ci_mq_failed_check_names: None,
            ci_mq_failure_fingerprint: None,
            ci_mq_same_signature_count: None,
            ci_mq_first_seen_at: None,
            ci_mq_last_seen_at: None,
            unresolved_blocker_count: 0,
            refinement_run_id: None,
            refinement_intent_id: None,
            refinement_generation: None,
            refinement_round: None,
            refinement_phase: None,
            refinement_role: None,
        }
    }

    fn fake_spec() -> TaskRunSpec {
        TaskRunSpec {
            task_run_id: "run-t1".into(),
            task_attempt_id: None,
            task_id: "t1".into(),
            project_id: "p1".into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/t1".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
            read_source_project_ids: Vec::new(),
            knowledge_injection: djinn_core::models::KnowledgeInjectionConfig::default(),
            github_owner: None,
            github_install_token: None,
            commit_author_name: None,
            commit_author_email: None,
            resume_lifecycle_metadata: None,
            is_evidence_spike: false,
        }
    }

    #[test]
    fn load_task_request_roundtrip() {
        let f = Frame {
            correlation_id: 42,
            payload: FramePayload::Rpc(ServiceRpcRequest::LoadTask {
                task_id: "t1".into(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.correlation_id, 42);
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::LoadTask { task_id }) => {
                assert_eq!(task_id, "t1");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn execute_stage_request_roundtrip() {
        let workspace = WorkspaceRef {
            path: PathBuf::from("/workspace"),
            branch: "djinn/t1".into(),
            owned_by_runtime: true,
        };
        let req = ServiceRpcRequest::ExecuteStage {
            task: fake_task(),
            workspace: workspace.clone(),
            role_kind: RoleKind::Planner,
            task_run_id: "run-1".into(),
            spec: fake_spec(),
        };
        let f = Frame {
            correlation_id: 7,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::ExecuteStage {
                workspace: w,
                role_kind,
                ..
            }) => {
                assert_eq!(w.path, workspace.path);
                assert!(matches!(role_kind, RoleKind::Planner));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn open_pr_request_roundtrip() {
        let req = ServiceRpcRequest::OpenPr {
            spec: fake_spec(),
            task: fake_task(),
        };
        let f = Frame {
            correlation_id: 9,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert!(matches!(
            back.payload,
            FramePayload::Rpc(ServiceRpcRequest::OpenPr { .. })
        ));
    }

    #[test]
    fn load_task_reply_roundtrip_ok() {
        let resp = ServiceRpcResponse::LoadTask(Ok(fake_task()));
        let f = Frame {
            correlation_id: 1,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::LoadTask(Ok(task))) => {
                assert_eq!(task.id, "t1");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn execute_stage_reply_err_roundtrip() {
        let resp = ServiceRpcResponse::ExecuteStage(Err(StageError::Setup("no such role".into())));
        let f = Frame {
            correlation_id: 2,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::ExecuteStage(Err(StageError::Setup(
                msg,
            )))) => {
                assert_eq!(msg, "no such role");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn execute_stage_reply_failed_throttle_roundtrip() {
        // A6: the `StageOutcome::Failed { provider_failure: Some(Throttle {
        // retry_after_ms }) }` shape must survive the bincode RPC frame so the
        // host can floor the redispatch cooldown on a provider-stated reset.
        let resp = ServiceRpcResponse::ExecuteStage(Ok(StageOutcome::Failed {
            reason: "rate limited".into(),
            provider_failure: Some(djinn_runtime::ProviderFailureClass::Throttle {
                retry_after_ms: Some(5 * 60 * 60 * 1000),
            }),
        }));
        let f = Frame {
            correlation_id: 3,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::ExecuteStage(Ok(
                StageOutcome::Failed {
                    reason,
                    provider_failure,
                },
            ))) => {
                assert_eq!(reason, "rate limited");
                assert_eq!(
                    provider_failure,
                    Some(djinn_runtime::ProviderFailureClass::Throttle {
                        retry_after_ms: Some(5 * 60 * 60 * 1000)
                    })
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn control_cancel_roundtrip() {
        let f = Frame {
            correlation_id: 0,
            payload: FramePayload::Control(ControlMsg::Cancel),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert!(matches!(
            back.payload,
            FramePayload::Control(ControlMsg::Cancel)
        ));
    }

    #[test]
    fn auth_hello_roundtrip() {
        let f = Frame {
            correlation_id: 0,
            payload: FramePayload::AuthHello(AuthHelloMsg {
                task_run_id: "run-7".into(),
                token: "kubeSA-bearer-xyz".into(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::AuthHello(AuthHelloMsg { task_run_id, token }) => {
                assert_eq!(task_run_id, "run-7");
                assert_eq!(token, "kubeSA-bearer-xyz");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn watchdog_termination_request_roundtrip_preserves_exact_identities() {
        let request = WatchdogTerminationRequest {
            task_id: "task-immutable".into(),
            task_run_id: "run-immutable".into(),
            pod_uid: "pod-immutable".into(),
        };
        let frame = Frame {
            correlation_id: 77,
            payload: FramePayload::Rpc(ServiceRpcRequest::TerminateWatchdogPod {
                request: request.clone(),
            }),
        };
        let back: Frame = bincode::deserialize(&bincode::serialize(&frame).unwrap()).unwrap();
        assert_eq!(back.correlation_id, 77);
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::TerminateWatchdogPod { request: got }) => {
                assert_eq!(got, request)
            }
            other => panic!("unexpected: {other:?}"),
        }
        let reply = Frame {
            correlation_id: 77,
            payload: FramePayload::RpcReply(ServiceRpcResponse::TerminateWatchdogPod(Err(
                "unconfirmed".into(),
            ))),
        };
        let back: Frame = bincode::deserialize(&bincode::serialize(&reply).unwrap()).unwrap();
        assert!(
            matches!(back.payload, FramePayload::RpcReply(ServiceRpcResponse::TerminateWatchdogPod(Err(ref e))) if e == "unconfirmed")
        );
    }

    #[test]
    fn create_task_run_request_roundtrip() {
        let params = SerializableCreateTaskRunParams {
            id: "run-create-1".into(),
            task_attempt_id: Some("attempt-2".into()),
            project_id: "p1".into(),
            task_id: "t1".into(),
            trigger_type: "new_task".into(),
            status: Some("running".into()),
            workspace_path: Some("/workspace".into()),
            mirror_ref: Some("refs/mirror/p1".into()),
            dispatch_owner_incarnation_id: Some("00000000-0000-7000-8000-000000000001".into()),
            dispatch_group_id: Some("00000000-0000-7000-8000-000000000002".into()),
        };
        let f = Frame {
            correlation_id: 11,
            payload: FramePayload::Rpc(ServiceRpcRequest::CreateTaskRun {
                params: params.clone(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.correlation_id, 11);
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::CreateTaskRun { params: got }) => {
                assert_eq!(got, params);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn create_task_run_reply_roundtrip() {
        let resp = ServiceRpcResponse::CreateTaskRun(Ok(()));
        let f = Frame {
            correlation_id: 11,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::CreateTaskRun(Ok(()))) => {}
            other => panic!("unexpected: {other:?}"),
        }

        // Err branch too.
        let resp = ServiceRpcResponse::CreateTaskRun(Err("duplicate id".into()));
        let f = Frame {
            correlation_id: 11,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::CreateTaskRun(Err(e))) => {
                assert_eq!(e, "duplicate id");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn update_task_run_status_request_roundtrip() {
        let f = Frame {
            correlation_id: 12,
            payload: FramePayload::Rpc(ServiceRpcRequest::UpdateTaskRunStatus {
                run_id: "run-1".into(),
                status: TaskRunStatus::Completed,
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.correlation_id, 12);
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::UpdateTaskRunStatus { run_id, status }) => {
                assert_eq!(run_id, "run-1");
                assert_eq!(status, TaskRunStatus::Completed);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn update_task_run_status_reply_roundtrip() {
        let resp = ServiceRpcResponse::UpdateTaskRunStatus(Ok(()));
        let f = Frame {
            correlation_id: 12,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::UpdateTaskRunStatus(Ok(()))) => {}
            other => panic!("unexpected: {other:?}"),
        }

        let resp = ServiceRpcResponse::UpdateTaskRunStatus(Err("not found".into()));
        let f = Frame {
            correlation_id: 12,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::UpdateTaskRunStatus(Err(e))) => {
                assert_eq!(e, "not found");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn get_model_context_window_request_roundtrip() {
        let f = Frame {
            correlation_id: 21,
            payload: FramePayload::Rpc(ServiceRpcRequest::GetModelContextWindow {
                model_id: "anthropic/claude-opus-4-7".into(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.correlation_id, 21);
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::GetModelContextWindow { model_id }) => {
                assert_eq!(model_id, "anthropic/claude-opus-4-7");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn get_model_context_window_reply_roundtrip() {
        let resp = ServiceRpcResponse::GetModelContextWindow(Ok(200_000));
        let f = Frame {
            correlation_id: 21,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::GetModelContextWindow(Ok(n))) => {
                assert_eq!(n, 200_000);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let resp = ServiceRpcResponse::GetModelContextWindow(Err("not found".into()));
        let f = Frame {
            correlation_id: 21,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::GetModelContextWindow(Err(e))) => {
                assert_eq!(e, "not found");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn get_provider_base_url_request_roundtrip() {
        let f = Frame {
            correlation_id: 22,
            payload: FramePayload::Rpc(ServiceRpcRequest::GetProviderBaseUrl {
                catalog_provider_id: "anthropic".into(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.correlation_id, 22);
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::GetProviderBaseUrl {
                catalog_provider_id,
            }) => {
                assert_eq!(catalog_provider_id, "anthropic");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn get_provider_base_url_reply_roundtrip() {
        let resp = ServiceRpcResponse::GetProviderBaseUrl(Ok("https://api.anthropic.com".into()));
        let f = Frame {
            correlation_id: 22,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::GetProviderBaseUrl(Ok(u))) => {
                assert_eq!(u, "https://api.anthropic.com");
            }
            other => panic!("unexpected: {other:?}"),
        }

        let resp = ServiceRpcResponse::GetProviderBaseUrl(Err("no such provider".into()));
        let f = Frame {
            correlation_id: 22,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::GetProviderBaseUrl(Err(e))) => {
                assert_eq!(e, "no such provider");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn pick_any_default_model_request_roundtrip() {
        let f = Frame {
            correlation_id: 23,
            payload: FramePayload::Rpc(ServiceRpcRequest::PickAnyDefaultModel),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.correlation_id, 23);
        assert!(matches!(
            back.payload,
            FramePayload::Rpc(ServiceRpcRequest::PickAnyDefaultModel)
        ));
    }

    #[test]
    fn pick_any_default_model_reply_roundtrip() {
        let resp = ServiceRpcResponse::PickAnyDefaultModel(Ok(Some("openai/gpt-4o-mini".into())));
        let f = Frame {
            correlation_id: 23,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::PickAnyDefaultModel(Ok(Some(m)))) => {
                assert_eq!(m, "openai/gpt-4o-mini");
            }
            other => panic!("unexpected: {other:?}"),
        }

        let resp = ServiceRpcResponse::PickAnyDefaultModel(Ok(None));
        let f = Frame {
            correlation_id: 23,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert!(matches!(
            back.payload,
            FramePayload::RpcReply(ServiceRpcResponse::PickAnyDefaultModel(Ok(None)))
        ));
    }

    #[test]
    fn create_session_request_roundtrip() {
        let params = SerializableCreateSessionParams {
            project_id: "p1".into(),
            task_id: Some("t1".into()),
            model: "anthropic/claude-opus-4-7".into(),
            agent_type: "planner".into(),
            metadata_json: None,
            task_run_id: Some("run-1".into()),
            cost_basis_hint: None,
            billing_source: None,
        };
        let f = Frame {
            correlation_id: 31,
            payload: FramePayload::Rpc(ServiceRpcRequest::CreateSession {
                params: params.clone(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.correlation_id, 31);
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::CreateSession { params: got }) => {
                assert_eq!(got, params);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn create_session_reply_roundtrip() {
        let session = SessionRecord {
            id: "s1".into(),
            project_id: Some("p1".into()),
            task_id: Some("t1".into()),
            model_id: "anthropic/claude-opus-4-7".into(),
            agent_type: "planner".into(),
            started_at: "2026-05-18T00:00:00Z".into(),
            ended_at: None,
            status: "running".into(),
            tokens_in: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            task_run_id: Some("run-1".into()),
            title: None,
            parked_reason: None,
            cost_usd: None,
            input_price_per_million_snapshot: None,
            output_price_per_million_snapshot: None,
            cache_read_price_per_million_snapshot: None,
            cache_write_price_per_million_snapshot: None,
            cost_basis: "unpriced".into(),
            billing_source: None,
        };
        let resp = ServiceRpcResponse::CreateSession(Ok(session.clone()));
        let f = Frame {
            correlation_id: 31,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::CreateSession(Ok(got))) => {
                assert_eq!(got.id, "s1");
                assert_eq!(got.task_run_id.as_deref(), Some("run-1"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn publish_session_message_request_roundtrip() {
        // Use a non-trivial nested payload to exercise the untagged-enum
        // internals trap — `serde_json::Value` is bincode-fatal when
        // shipped directly, hence the opaque-JSON-string wire shape.
        let msg = serde_json::json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "input": {"nested": {"array": [1, 2, 3]}}},
            ],
        });
        let msg_str = msg.to_string();
        let f = Frame {
            correlation_id: 32,
            payload: FramePayload::Rpc(ServiceRpcRequest::PublishSessionMessage {
                session_id: "s1".into(),
                task_id: "t1".into(),
                agent_type: "worker".into(),
                message: msg_str.clone(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).expect(
            "PublishSessionMessage with nested JSON payload must roundtrip via bincode \
             (regression guard for the serde_json::Value DeserializeAnyNotSupported trap)",
        );
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::PublishSessionMessage {
                session_id,
                task_id,
                agent_type,
                message,
            }) => {
                assert_eq!(session_id, "s1");
                assert_eq!(task_id, "t1");
                assert_eq!(agent_type, "worker");
                assert_eq!(message, msg_str);
                // Re-parse the wire-shipped string back into a Value and
                // assert the structural equivalence the host would see.
                let parsed: serde_json::Value = serde_json::from_str(&message).unwrap();
                assert_eq!(parsed, msg);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn publish_session_message_reply_roundtrip() {
        let resp = ServiceRpcResponse::PublishSessionMessage(Ok(()));
        let f = Frame {
            correlation_id: 32,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert!(matches!(
            back.payload,
            FramePayload::RpcReply(ServiceRpcResponse::PublishSessionMessage(Ok(())))
        ));
    }

    #[test]
    fn get_environment_config_request_roundtrip() {
        let f = Frame {
            correlation_id: 33,
            payload: FramePayload::Rpc(ServiceRpcRequest::GetEnvironmentConfig {
                project_id: "p1".into(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::GetEnvironmentConfig { project_id }) => {
                assert_eq!(project_id, "p1");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn get_environment_config_reply_roundtrip() {
        // Pre-fix, this worked only because `EnvironmentConfig::empty()`
        // ships zero `HookCommand`s. Build a config WITH HookCommand
        // variants (Shell, Exec) for post_build/pre_anything and a
        // PreTaskCommand for pre_task to prove the opaque-JSON wire shape
        // survives bincode round-trip.
        use djinn_stack::environment::{
            EnvironmentConfig, HookCommand, LifecycleHooks, PreTaskCommand,
        };
        let cfg = EnvironmentConfig {
            lifecycle: LifecycleHooks {
                post_build: vec![HookCommand::Shell("echo build".into())],
                pre_anything: vec![HookCommand::Exec(vec![
                    "bash".into(),
                    "-lc".into(),
                    "echo ready".into(),
                ])],
                pre_task: vec![PreTaskCommand {
                    name: Some("install".into()),
                    command: "pip install -e .".into(),
                    timeout_seconds: 300,
                    failure_policy: Default::default(),
                }],
                pre_verification: vec![],
                ..Default::default()
            },
            ..EnvironmentConfig::empty()
        };
        let cfg_json = serde_json::to_string(&cfg).expect("encode EnvironmentConfig");
        let resp = ServiceRpcResponse::GetEnvironmentConfig(Ok(cfg_json.clone()));
        let f = Frame {
            correlation_id: 33,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).expect(
            "GetEnvironmentConfig with non-empty hook lists must roundtrip via bincode \
             (regression guard for the #[serde(untagged)] HookCommand DeserializeAnyNotSupported trap)",
        );
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::GetEnvironmentConfig(Ok(got))) => {
                assert_eq!(got, cfg_json);
                let cfg_back: EnvironmentConfig =
                    serde_json::from_str(&got).expect("re-parse config");
                assert_eq!(cfg_back.lifecycle.post_build.len(), 1);
                assert_eq!(cfg_back.lifecycle.pre_anything.len(), 1);
                assert_eq!(cfg_back.lifecycle.pre_task.len(), 1);
                assert_eq!(cfg_back.lifecycle.pre_task[0].command, "pip install -e .");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn invoke_llm_request_roundtrip() {
        use djinn_provider::message::{Conversation, Message};
        use djinn_provider::provider::ToolChoice;
        // Build a conversation with a ToolUse content block — the
        // internally-tagged ContentBlock + ToolUse.input Value combo is
        // the exact shape that broke bincode pre-fix.
        let mut conv = Conversation::new();
        conv.push(Message::user("hello"));
        conv.push(Message {
            role: djinn_core::message::Role::Assistant,
            content: vec![djinn_core::message::ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "lookup".into(),
                input: serde_json::json!({"nested": {"array": [1, 2, 3]}}),
            }],
            metadata: None,
        });
        let conv_str = serde_json::to_string(&conv).unwrap();
        let tools_value = vec![serde_json::json!({
            "name": "lookup",
            "schema": {"type": "object", "properties": {"x": {"type": "string"}}},
        })];
        let tools_str = serde_json::to_string(&tools_value).unwrap();
        let f = Frame {
            correlation_id: 41,
            payload: FramePayload::Rpc(ServiceRpcRequest::InvokeLlm {
                model_id: "anthropic/claude-opus-4-7".into(),
                conversation: conv_str.clone(),
                tools: tools_str.clone(),
                tool_choice: Some(ToolChoice::Auto),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).expect(
            "InvokeLlm with nested Conversation/tools must roundtrip via bincode \
             (regression guard for ContentBlock + serde_json::Value DeserializeAnyNotSupported)",
        );
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::InvokeLlm {
                model_id,
                conversation,
                tools,
                tool_choice,
            }) => {
                assert_eq!(model_id, "anthropic/claude-opus-4-7");
                assert_eq!(conversation, conv_str);
                assert_eq!(tools, tools_str);
                assert_eq!(tool_choice, Some(ToolChoice::Auto));
                // Re-parse and confirm structural fidelity end-to-end.
                let conv_back: Conversation = serde_json::from_str(&conversation).unwrap();
                assert_eq!(conv_back.len(), 2);
                let tools_back: Vec<serde_json::Value> = serde_json::from_str(&tools).unwrap();
                assert_eq!(tools_back, tools_value);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn invoke_llm_reply_roundtrip() {
        use djinn_core::message::ContentBlock;
        use djinn_provider::provider::{LlmResponse, TokenUsage};
        // Build a non-trivial response with a ToolUse block — proves the
        // opaque-JSON-string wire shape survives the ContentBlock +
        // Value combo that broke before.
        let llm_resp = LlmResponse {
            content: vec![
                ContentBlock::text("hi back"),
                ContentBlock::ToolUse {
                    id: "call_2".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"k": [1, 2]}),
                },
            ],
            thinking: String::new(),
            usage: TokenUsage {
                input: 12,
                output: 7,
                ..Default::default()
            },
        };
        let payload_str = serde_json::to_string(&llm_resp).unwrap();
        let resp = ServiceRpcResponse::InvokeLlm(Ok(payload_str.clone()));
        let f = Frame {
            correlation_id: 41,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::InvokeLlm(Ok(got))) => {
                assert_eq!(got, payload_str);
                let r: LlmResponse = serde_json::from_str(&got).unwrap();
                assert_eq!(r.usage.input, 12);
                assert_eq!(r.usage.output, 7);
                assert_eq!(r.content.len(), 2);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn update_session_status_request_roundtrip() {
        for parked_reason in [Some("budget".to_string()), None] {
            let f = Frame {
                correlation_id: 42,
                payload: FramePayload::Rpc(ServiceRpcRequest::UpdateSessionStatus {
                    session_id: "s1".into(),
                    status: SessionStatus::Completed,
                    tokens_in: 1234,
                    tokens_out: 567,
                    cache_read: 89,
                    cache_write: 12,
                    parked_reason: parked_reason.clone(),
                }),
            };
            let bytes = bincode::serialize(&f).unwrap();
            let back: Frame = bincode::deserialize(&bytes).unwrap();
            match back.payload {
                FramePayload::Rpc(ServiceRpcRequest::UpdateSessionStatus {
                    session_id,
                    status,
                    tokens_in,
                    tokens_out,
                    cache_read,
                    cache_write,
                    parked_reason: got_parked_reason,
                }) => {
                    assert_eq!(session_id, "s1");
                    assert_eq!(status, SessionStatus::Completed);
                    assert_eq!(tokens_in, 1234);
                    assert_eq!(tokens_out, 567);
                    assert_eq!(cache_read, 89);
                    assert_eq!(cache_write, 12);
                    assert_eq!(got_parked_reason, parked_reason);
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    #[test]
    fn update_session_status_reply_roundtrip() {
        for parked_reason in [Some("budget".to_string()), None] {
            let request = Frame {
                correlation_id: 42,
                payload: FramePayload::Rpc(ServiceRpcRequest::UpdateSessionStatus {
                    session_id: "s1".into(),
                    status: SessionStatus::Completed,
                    tokens_in: 1234,
                    tokens_out: 567,
                    cache_read: 89,
                    cache_write: 12,
                    parked_reason: parked_reason.clone(),
                }),
            };
            let bytes = bincode::serialize(&request).unwrap();
            let back: Frame = bincode::deserialize(&bytes).unwrap();
            match back.payload {
                FramePayload::Rpc(ServiceRpcRequest::UpdateSessionStatus {
                    parked_reason: got_parked_reason,
                    ..
                }) => assert_eq!(got_parked_reason, parked_reason),
                other => panic!("unexpected: {other:?}"),
            }

            let response = Frame {
                correlation_id: 43,
                payload: FramePayload::RpcReply(ServiceRpcResponse::UpdateSessionStatus(Ok(()))),
            };
            let bytes = bincode::serialize(&response).unwrap();
            let back: Frame = bincode::deserialize(&bytes).unwrap();
            match back.payload {
                FramePayload::RpcReply(ServiceRpcResponse::UpdateSessionStatus(got)) => {
                    assert_eq!(got, Ok(()));
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    #[test]
    fn emit_djinn_event_request_roundtrip() {
        let payload_str = serde_json::json!({
            "session_id":"s1","task_id":"t1","agent_type":"worker","message":{"role":"assistant"}
        })
        .to_string();
        let event = SerializableDjinnEvent {
            entity_type: "session".into(),
            action: "message".into(),
            payload: payload_str,
            id: None,
            project_id: Some("p1".into()),
        };
        let f = Frame {
            correlation_id: 51,
            payload: FramePayload::Rpc(ServiceRpcRequest::EmitDjinnEvent {
                event: event.clone(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::EmitDjinnEvent { event: got }) => {
                assert_eq!(got, event);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// The load-bearing roundtrip: before the fix, `id: None` +
    /// `project_id: None` (as emitted by `activity.logged` envelopes
    /// during worker teardown) silently produced an under-sized bincode
    /// body that deserialised as "unexpected end of file". The
    /// `skip_serializing_if` attribute is JSON-only; bincode is positional
    /// and needs every field slot populated. Keep this test as the canary.
    #[test]
    fn emit_djinn_event_request_roundtrip_with_both_options_none() {
        let payload_str =
            serde_json::json!({"kind": "activity", "msg": "tearing down"}).to_string();
        let event = SerializableDjinnEvent {
            entity_type: "activity".into(),
            action: "logged".into(),
            payload: payload_str,
            id: None,
            project_id: None,
        };
        let f = Frame {
            correlation_id: 99,
            payload: FramePayload::Rpc(ServiceRpcRequest::EmitDjinnEvent {
                event: event.clone(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).expect(
            "EmitDjinnEvent with id=None, project_id=None must roundtrip via bincode \
             (regression guard for the JSON-only skip_serializing_if attribute)",
        );
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::EmitDjinnEvent { event: got }) => {
                assert_eq!(got, event);
                assert!(got.id.is_none());
                assert!(got.project_id.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn emit_djinn_event_reply_roundtrip() {
        let resp = ServiceRpcResponse::EmitDjinnEvent(Ok(()));
        let f = Frame {
            correlation_id: 51,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert!(matches!(
            back.payload,
            FramePayload::RpcReply(ServiceRpcResponse::EmitDjinnEvent(Ok(())))
        ));
    }

    #[test]
    fn tool_github_search_roundtrip() {
        // Exercise the untagged-enum-internals trap with a nested
        // argument shape that a Value field would have silently corrupted.
        let args_value = serde_json::json!({
            "query": "fn foo",
            "filters": {"language": "rust", "nested": [1, 2, 3]},
        });
        let args_str = args_value.to_string();
        let f = Frame {
            correlation_id: 61,
            payload: FramePayload::Rpc(ServiceRpcRequest::ToolGithubSearch {
                project_id: Some("p1".into()),
                arguments: args_str.clone(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).expect(
            "ToolGithubSearch with nested JSON arguments must roundtrip via bincode \
             (regression guard for the serde_json::Map DeserializeAnyNotSupported trap)",
        );
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::ToolGithubSearch {
                project_id,
                arguments,
            }) => {
                assert_eq!(project_id.as_deref(), Some("p1"));
                let parsed: serde_json::Value = serde_json::from_str(&arguments).unwrap();
                assert_eq!(parsed, args_value);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Response leg — opaque-JSON-encoded Ok payload survives bincode.
        let resp_payload = serde_json::json!({"items": [{"name": "foo"}]}).to_string();
        let resp = ServiceRpcResponse::ToolGithubSearch(Ok(resp_payload.clone()));
        let f = Frame {
            correlation_id: 61,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::ToolGithubSearch(Ok(got))) => {
                assert_eq!(got, resp_payload);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tool_github_fetch_file_roundtrip() {
        let args_value = serde_json::json!({
            "repo": "octocat/Hello",
            "path": "README.md",
            "options": {"ref": "main"},
        });
        let args_str = args_value.to_string();
        let f = Frame {
            correlation_id: 62,
            payload: FramePayload::Rpc(ServiceRpcRequest::ToolGithubFetchFile {
                project_id: None,
                arguments: args_str.clone(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::ToolGithubFetchFile {
                project_id,
                arguments,
            }) => {
                assert_eq!(project_id, None);
                let parsed: serde_json::Value = serde_json::from_str(&arguments).unwrap();
                assert_eq!(parsed, args_value);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tool_ci_job_log_roundtrip() {
        let args_value = serde_json::json!({
            "job_id": 12345,
            "filters": {"levels": ["error", "warn"]},
        });
        let args_str = args_value.to_string();
        let f = Frame {
            correlation_id: 63,
            payload: FramePayload::Rpc(ServiceRpcRequest::ToolCiJobLog {
                session_task_id: Some("t1".into()),
                arguments: args_str.clone(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::ToolCiJobLog {
                session_task_id,
                arguments,
            }) => {
                assert_eq!(session_task_id.as_deref(), Some("t1"));
                let parsed: serde_json::Value = serde_json::from_str(&arguments).unwrap();
                assert_eq!(parsed, args_value);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn serializable_djinn_event_from_envelope() {
        use djinn_core::events::DjinnEventEnvelope;
        let env = DjinnEventEnvelope::session_message(
            "s1",
            "t1",
            "worker",
            &serde_json::json!({"role": "assistant"}),
        );
        let wire = SerializableDjinnEvent::from_envelope(&env);
        assert_eq!(wire.entity_type, "session");
        assert_eq!(wire.action, "message");
        // `payload` is an opaque JSON string on the wire — re-parse to
        // assert the underlying shape rather than index a `Value` directly.
        let parsed: serde_json::Value =
            serde_json::from_str(&wire.payload).expect("payload is valid JSON");
        assert_eq!(parsed["session_id"], "s1");
        assert_eq!(wire.id, None);
        assert_eq!(wire.project_id, None);
    }

    #[test]
    fn touch_activity_request_roundtrip() {
        let f = Frame {
            correlation_id: 71,
            payload: FramePayload::Rpc(ServiceRpcRequest::TouchActivity {
                task_id: "task-77".into(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.correlation_id, 71);
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::TouchActivity { task_id }) => {
                assert_eq!(task_id, "task-77");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn touch_activity_reply_roundtrip() {
        let resp = ServiceRpcResponse::TouchActivity(Ok(()));
        let f = Frame {
            correlation_id: 71,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert!(matches!(
            back.payload,
            FramePayload::RpcReply(ServiceRpcResponse::TouchActivity(Ok(())))
        ));

        let resp = ServiceRpcResponse::TouchActivity(Err("unknown task".into()));
        let f = Frame {
            correlation_id: 71,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::TouchActivity(Err(e))) => {
                assert_eq!(e, "unknown task");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn transition_task_request_roundtrip() {
        let f = Frame {
            correlation_id: 99,
            payload: FramePayload::Rpc(ServiceRpcRequest::TransitionTask {
                task_id: "task-77".into(),
                action: "task_review_approve".into(),
                reason: Some("looks good".into()),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.correlation_id, 99);
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::TransitionTask {
                task_id,
                action,
                reason,
            }) => {
                assert_eq!(task_id, "task-77");
                assert_eq!(action, "task_review_approve");
                assert_eq!(reason.as_deref(), Some("looks good"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn transition_task_reply_roundtrip() {
        let resp = ServiceRpcResponse::TransitionTask(Ok(()));
        let f = Frame {
            correlation_id: 99,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert!(matches!(
            back.payload,
            FramePayload::RpcReply(ServiceRpcResponse::TransitionTask(Ok(())))
        ));

        let resp = ServiceRpcResponse::TransitionTask(Err("InvalidTransition".into()));
        let f = Frame {
            correlation_id: 99,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::TransitionTask(Err(e))) => {
                assert_eq!(e, "InvalidTransition");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn auth_result_roundtrip() {
        let f = Frame {
            correlation_id: 0,
            payload: FramePayload::AuthResult(AuthResultMsg {
                accepted: false,
                error: Some("invalid bearer token".into()),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::AuthResult(AuthResultMsg { accepted, error }) => {
                assert!(!accepted);
                assert_eq!(error.as_deref(), Some("invalid bearer token"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn publish_branch_to_github_request_roundtrip() {
        let req = ServiceRpcRequest::PublishBranchToGithub {
            spec: fake_spec(),
            task: fake_task(),
        };
        let f = Frame {
            correlation_id: 88,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.correlation_id, 88);
        assert!(matches!(
            back.payload,
            FramePayload::Rpc(ServiceRpcRequest::PublishBranchToGithub { .. })
        ));
    }

    #[test]
    fn publish_branch_to_github_reply_roundtrip() {
        let resp = ServiceRpcResponse::PublishBranchToGithub(BranchPublicationResult {
            success: true,
            pushed_sha: Some("abc123".into()),
            mirror_head: "mirror_sha".into(),
            attempted_github_head: "abc123".into(),
            pr_branch_existed: true,
            error_class: None,
            error_message: None,
        });
        let f = Frame {
            correlation_id: 88,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::PublishBranchToGithub(result)) => {
                assert!(result.success);
                assert_eq!(result.pushed_sha.as_deref(), Some("abc123"));
                assert_eq!(result.mirror_head, "mirror_sha");
                assert!(result.pr_branch_existed);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Failure case roundtrip.
        let resp = ServiceRpcResponse::PublishBranchToGithub(BranchPublicationResult {
            success: false,
            pushed_sha: None,
            mirror_head: "mirror_sha".into(),
            attempted_github_head: "attempted_sha".into(),
            pr_branch_existed: true,
            error_class: Some("push_rejected".into()),
            error_message: Some("force-push rejected".into()),
        });
        let f = Frame {
            correlation_id: 89,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::PublishBranchToGithub(result)) => {
                assert!(!result.success);
                assert_eq!(result.error_class.as_deref(), Some("push_rejected"));
                assert_eq!(result.error_message.as_deref(), Some("force-push rejected"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn ci_artifact_request_roundtrip() {
        let req = ServiceRpcRequest::ToolCiArtifact {
            session_task_id: Some("t1".into()),
            arguments: r#"{"action":"list"}"#.into(),
        };
        let f = Frame {
            correlation_id: 99,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::ToolCiArtifact {
                session_task_id,
                arguments,
            }) => {
                assert_eq!(session_task_id.as_deref(), Some("t1"));
                assert_eq!(arguments, r#"{"action":"list"}"#);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn ci_artifact_response_roundtrip() {
        let resp =
            ServiceRpcResponse::ToolCiArtifact(Ok(r#"{"run_id":42,"lane":"explicit"}"#.into()));
        let f = Frame {
            correlation_id: 100,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::ToolCiArtifact(Ok(payload))) => {
                assert!(payload.contains("run_id"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ── Lease-v1 wire serialization round-trips ───────────────────────

    use crate::services::lease::*;

    fn task_invocation_identity() -> LeaseIdentity {
        LeaseIdentity::TaskInvocation(TaskInvocationLeaseIdentity {
            task_id: "task-1".into(),
            task_run_id: "run-1".into(),
            invocation_id: "inv-1".into(),
        })
    }

    fn graph_warm_identity() -> LeaseIdentity {
        LeaseIdentity::GraphWarm(GraphWarmLeaseIdentity {
            project_id: "proj-1".into(),
            warm_request_id: "warm-1".into(),
            graph_revision: "rev-42".into(),
        })
    }

    fn sample_deadlines() -> LeaseDeadlines {
        LeaseDeadlines {
            queue_deadline_ms: 60_000,
            launch_deadline_ms: 120_000,
        }
    }

    #[test]
    fn lease_queue_request_roundtrip() {
        // Task-invocation identity
        let req = ServiceRpcRequest::QueueLease {
            request: LeaseQueueRequest {
                identity: task_invocation_identity(),
                deadlines: sample_deadlines(),
            },
        };
        let f = Frame {
            correlation_id: 50,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::QueueLease { request }) => {
                match request.identity {
                    LeaseIdentity::TaskInvocation(ti) => {
                        assert_eq!(ti.task_id, "task-1");
                        assert_eq!(ti.invocation_id, "inv-1");
                    }
                    other => panic!("unexpected identity: {other:?}"),
                }
                assert_eq!(request.deadlines.queue_deadline_ms, 60_000);
                assert_eq!(request.deadlines.launch_deadline_ms, 120_000);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Graph-warm identity
        let req = ServiceRpcRequest::QueueLease {
            request: LeaseQueueRequest {
                identity: graph_warm_identity(),
                deadlines: sample_deadlines(),
            },
        };
        let f = Frame {
            correlation_id: 51,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::QueueLease { request }) => {
                match request.identity {
                    LeaseIdentity::GraphWarm(gw) => {
                        assert_eq!(gw.project_id, "proj-1");
                        assert_eq!(gw.warm_request_id, "warm-1");
                        assert_eq!(gw.graph_revision, "rev-42");
                    }
                    other => panic!("unexpected identity: {other:?}"),
                }
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_grant_request_roundtrip() {
        let req = ServiceRpcRequest::GrantLease {
            request: LeaseGrantRequest {
                identity: task_invocation_identity(),
                fencing_token: LeaseFencingToken(7),
            },
        };
        let f = Frame {
            correlation_id: 52,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::GrantLease { request }) => {
                assert_eq!(request.fencing_token, LeaseFencingToken(7));
                assert!(matches!(request.identity, LeaseIdentity::TaskInvocation(_)));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_status_request_roundtrip() {
        let req = ServiceRpcRequest::LeaseStatus {
            request: LeaseStatusRequest {
                identity: graph_warm_identity(),
            },
        };
        let f = Frame {
            correlation_id: 53,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::LeaseStatus { request }) => {
                assert!(matches!(request.identity, LeaseIdentity::GraphWarm(_)));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_abandon_request_roundtrip() {
        let req = ServiceRpcRequest::AbandonLease {
            request: LeaseAbandonRequest {
                identity: task_invocation_identity(),
                candidate_cleanup: true,
            },
        };
        let f = Frame {
            correlation_id: 54,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::AbandonLease { request }) => {
                assert!(request.candidate_cleanup);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_bind_request_roundtrip() {
        let req = ServiceRpcRequest::BindLeasePod {
            request: LeaseBindRequest {
                identity: task_invocation_identity(),
                fencing_token: LeaseFencingToken(3),
                pod_uid: "pod-abc-123".into(),
            },
        };
        let f = Frame {
            correlation_id: 55,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::BindLeasePod { request }) => {
                assert_eq!(request.fencing_token, LeaseFencingToken(3));
                assert_eq!(request.pod_uid, "pod-abc-123");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_cancel_request_roundtrip() {
        // With fencing token
        let req = ServiceRpcRequest::CancelLease {
            request: LeaseCancelRequest {
                identity: task_invocation_identity(),
                fencing_token: Some(LeaseFencingToken(9)),
                candidate_cleanup: false,
            },
        };
        let f = Frame {
            correlation_id: 56,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::CancelLease { request }) => {
                assert_eq!(request.fencing_token, Some(LeaseFencingToken(9)));
                assert!(!request.candidate_cleanup);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Without fencing token (None)
        let req = ServiceRpcRequest::CancelLease {
            request: LeaseCancelRequest {
                identity: graph_warm_identity(),
                fencing_token: None,
                candidate_cleanup: true,
            },
        };
        let f = Frame {
            correlation_id: 57,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::CancelLease { request }) => {
                assert!(request.fencing_token.is_none());
                assert!(request.candidate_cleanup);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_release_request_roundtrip() {
        let req = ServiceRpcRequest::ReleaseLease {
            request: LeaseReleaseRequest {
                identity: task_invocation_identity(),
                fencing_token: LeaseFencingToken(5),
                candidate_cleanup: true,
            },
        };
        let f = Frame {
            correlation_id: 58,
            payload: FramePayload::Rpc(req),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::ReleaseLease { request }) => {
                assert_eq!(request.fencing_token, LeaseFencingToken(5));
                assert!(request.candidate_cleanup);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_queued_response_roundtrip() {
        let resp = ServiceRpcResponse::QueueLease(LeaseResult::Queued(LeaseStatus {
            state: LeaseState::Queued,
            fencing_token: None,
            deadlines: sample_deadlines(),
            pod_uid: None,
            candidate_cleanup: false,
        }));
        let f = Frame {
            correlation_id: 60,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::QueueLease(LeaseResult::Queued(status))) => {
                assert_eq!(status.state, LeaseState::Queued);
                assert!(status.fencing_token.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_granted_response_roundtrip() {
        let resp = ServiceRpcResponse::GrantLease(LeaseResult::Granted(LeaseGrant {
            fencing_token: LeaseFencingToken(42),
            deadlines: sample_deadlines(),
        }));
        let f = Frame {
            correlation_id: 61,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::GrantLease(LeaseResult::Granted(grant))) => {
                assert_eq!(grant.fencing_token, LeaseFencingToken(42));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_status_response_roundtrip() {
        let resp = ServiceRpcResponse::LeaseStatus(LeaseResult::Status(LeaseStatus {
            state: LeaseState::Active,
            fencing_token: Some(LeaseFencingToken(11)),
            deadlines: sample_deadlines(),
            pod_uid: Some("pod-xyz".into()),
            candidate_cleanup: true,
        }));
        let f = Frame {
            correlation_id: 62,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::LeaseStatus(LeaseResult::Status(
                status,
            ))) => {
                assert_eq!(status.state, LeaseState::Active);
                assert_eq!(status.fencing_token, Some(LeaseFencingToken(11)));
                assert_eq!(status.pod_uid.as_deref(), Some("pod-xyz"));
                assert!(status.candidate_cleanup);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_abandoned_response_roundtrip() {
        let resp = ServiceRpcResponse::AbandonLease(LeaseResult::Abandoned {
            candidate_cleanup: true,
        });
        let f = Frame {
            correlation_id: 63,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::AbandonLease(LeaseResult::Abandoned {
                candidate_cleanup,
            })) => {
                assert!(candidate_cleanup);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_bound_response_roundtrip() {
        let resp = ServiceRpcResponse::BindLeasePod(LeaseResult::Bound(LeaseStatus {
            state: LeaseState::Bound,
            fencing_token: Some(LeaseFencingToken(2)),
            deadlines: sample_deadlines(),
            pod_uid: Some("pod-bound".into()),
            candidate_cleanup: false,
        }));
        let f = Frame {
            correlation_id: 64,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::BindLeasePod(LeaseResult::Bound(
                status,
            ))) => {
                assert_eq!(status.state, LeaseState::Bound);
                assert_eq!(status.pod_uid.as_deref(), Some("pod-bound"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_cancelled_response_roundtrip() {
        let resp = ServiceRpcResponse::CancelLease(LeaseResult::Cancelled {
            candidate_cleanup: true,
        });
        let f = Frame {
            correlation_id: 65,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::CancelLease(LeaseResult::Cancelled {
                candidate_cleanup,
            })) => {
                assert!(candidate_cleanup);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_released_response_roundtrip() {
        let resp = ServiceRpcResponse::ReleaseLease(LeaseResult::Released {
            candidate_cleanup: false,
        });
        let f = Frame {
            correlation_id: 66,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::ReleaseLease(LeaseResult::Released {
                candidate_cleanup,
            })) => {
                assert!(!candidate_cleanup);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_identity_conflict_response_roundtrip() {
        let resp = ServiceRpcResponse::QueueLease(LeaseResult::LeaseIdentityConflict {
            identity: task_invocation_identity(),
        });
        let f = Frame {
            correlation_id: 67,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::QueueLease(
                LeaseResult::LeaseIdentityConflict { identity },
            )) => {
                assert!(matches!(identity, LeaseIdentity::TaskInvocation(_)));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_wait_timeout_response_roundtrip() {
        // With timeout credit
        let resp = ServiceRpcResponse::QueueLease(LeaseResult::LeaseWaitTimeout {
            timeout_credit: Some(TimeoutCredit {
                units: 3,
                retry_after_ms: 5_000,
            }),
        });
        let f = Frame {
            correlation_id: 68,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::QueueLease(
                LeaseResult::LeaseWaitTimeout { timeout_credit },
            )) => {
                let credit = timeout_credit.expect("credit");
                assert_eq!(credit.units, 3);
                assert_eq!(credit.retry_after_ms, 5_000);
            }
            other => panic!("unexpected: {other:?}"),
        }

        // Without timeout credit (None)
        let resp = ServiceRpcResponse::QueueLease(LeaseResult::LeaseWaitTimeout {
            timeout_credit: None,
        });
        let f = Frame {
            correlation_id: 69,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::QueueLease(
                LeaseResult::LeaseWaitTimeout { timeout_credit },
            )) => {
                assert!(timeout_credit.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn lease_unavailable_response_roundtrip() {
        let resp = ServiceRpcResponse::QueueLease(LeaseResult::LeaseUnavailable);
        let f = Frame {
            correlation_id: 70,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::QueueLease(
                LeaseResult::LeaseUnavailable,
            )) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ── Bincode discriminant regression ──────────────────────────────
    //
    // Bincode encodes enum variants by position. Inserting variants in the
    // middle shifts every subsequent discriminant and breaks mixed-version
    // host/worker frames. This test pins every variant to its expected index
    // so a future edit cannot silently renumber the wire.

    #[test]
    fn request_discriminant_indices_stable() {
        // The discriminant is encoded as a varint/U32 prefix on each
        // variant. We probe it by serialising a single-variant payload.
        // The first 4 bytes of bincode output for an enum with < 256
        // variants (default config, FixintEncoding) encode the variant
        // index as a little-endian u32.
        fn idx(req: ServiceRpcRequest) -> u32 {
            let bytes = bincode::serialize(&req).unwrap();
            u32::from_le_bytes(bytes[..4].try_into().unwrap())
        }

        // Pre-existing variants (must not shift).
        assert_eq!(
            idx(ServiceRpcRequest::LoadTask {
                task_id: String::new()
            }),
            0
        );
        // ExecuteStage requires a lot of fields — we skip it in the index
        // probe but assert the remaining pre-existing tail variants.
        assert_eq!(
            idx(ServiceRpcRequest::OpenPr {
                spec: fake_spec(),
                task: fake_task(),
            }),
            2
        );
        assert_eq!(
            idx(ServiceRpcRequest::CreateTaskRun {
                params: SerializableCreateTaskRunParams {
                    id: String::new(),
                    task_attempt_id: None,
                    project_id: String::new(),
                    task_id: String::new(),
                    trigger_type: String::new(),
                    status: None,
                    workspace_path: None,
                    mirror_ref: None,
                    dispatch_owner_incarnation_id: None,
                    dispatch_group_id: None,
                }
            }),
            3
        );
        assert_eq!(
            idx(ServiceRpcRequest::ToolCiArtifact {
                session_task_id: None,
                arguments: String::new(),
            }),
            27
        );

        // Lease-v1 variants appended at the tail (indices 28–34).
        assert_eq!(
            idx(ServiceRpcRequest::QueueLease {
                request: LeaseQueueRequest {
                    identity: task_invocation_identity(),
                    deadlines: sample_deadlines(),
                }
            }),
            28
        );
        assert_eq!(
            idx(ServiceRpcRequest::GrantLease {
                request: LeaseGrantRequest {
                    identity: task_invocation_identity(),
                    fencing_token: LeaseFencingToken(0),
                }
            }),
            29
        );
        assert_eq!(
            idx(ServiceRpcRequest::LeaseStatus {
                request: LeaseStatusRequest {
                    identity: task_invocation_identity(),
                }
            }),
            30
        );
        assert_eq!(
            idx(ServiceRpcRequest::AbandonLease {
                request: LeaseAbandonRequest {
                    identity: task_invocation_identity(),
                    candidate_cleanup: false,
                }
            }),
            31
        );
        assert_eq!(
            idx(ServiceRpcRequest::BindLeasePod {
                request: LeaseBindRequest {
                    identity: task_invocation_identity(),
                    fencing_token: LeaseFencingToken(0),
                    pod_uid: String::new(),
                }
            }),
            32
        );
        assert_eq!(
            idx(ServiceRpcRequest::CancelLease {
                request: LeaseCancelRequest {
                    identity: task_invocation_identity(),
                    fencing_token: None,
                    candidate_cleanup: false,
                }
            }),
            33
        );
        assert_eq!(
            idx(ServiceRpcRequest::ReleaseLease {
                request: LeaseReleaseRequest {
                    identity: task_invocation_identity(),
                    fencing_token: LeaseFencingToken(0),
                    candidate_cleanup: false,
                }
            }),
            34
        );
    }

    #[test]
    fn response_discriminant_indices_stable() {
        fn idx(resp: ServiceRpcResponse) -> u32 {
            let bytes = bincode::serialize(&resp).unwrap();
            u32::from_le_bytes(bytes[..4].try_into().unwrap())
        }

        // Pre-existing response variants.
        assert_eq!(idx(ServiceRpcResponse::LoadTask(Err(String::new()))), 0);
        assert_eq!(
            idx(ServiceRpcResponse::OpenPr(TaskRunOutcome::Closed {
                reason: String::new()
            })),
            2
        );
        assert_eq!(idx(ServiceRpcResponse::Err(String::new())), 19);
        assert_eq!(
            idx(ServiceRpcResponse::ToolCiArtifact(Err(String::new()))),
            28
        );

        // Lease-v1 response variants appended at the tail (indices 29–35).
        assert_eq!(
            idx(ServiceRpcResponse::QueueLease(
                LeaseResult::LeaseUnavailable
            )),
            29
        );
        assert_eq!(
            idx(ServiceRpcResponse::GrantLease(
                LeaseResult::LeaseUnavailable
            )),
            30
        );
        assert_eq!(
            idx(ServiceRpcResponse::LeaseStatus(
                LeaseResult::LeaseUnavailable
            )),
            31
        );
        assert_eq!(
            idx(ServiceRpcResponse::AbandonLease(
                LeaseResult::LeaseUnavailable
            )),
            32
        );
        assert_eq!(
            idx(ServiceRpcResponse::BindLeasePod(
                LeaseResult::LeaseUnavailable
            )),
            33
        );
        assert_eq!(
            idx(ServiceRpcResponse::CancelLease(
                LeaseResult::LeaseUnavailable
            )),
            34
        );
        assert_eq!(
            idx(ServiceRpcResponse::ReleaseLease(
                LeaseResult::LeaseUnavailable
            )),
            35
        );
    }
}
