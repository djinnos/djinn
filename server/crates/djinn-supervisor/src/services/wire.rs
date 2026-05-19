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
use djinn_stack::environment::EnvironmentConfig;
use serde::{Deserialize, Serialize};

use crate::{RoleKind, StageError, StageOutcome, TaskRunOutcome, TaskRunSpec};

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
    pub project_id: String,
    pub task_id: String,
    pub trigger_type: String,
    /// Initial status; `None` defaults to `"running"` on the host side.
    pub status: Option<String>,
    pub workspace_path: Option<String>,
    pub mirror_ref: Option<String>,
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
}

/// Contents of an [`FramePayload::AuthResult`] handshake reply.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthResultMsg {
    /// Whether the token was accepted.
    pub accepted: bool,
    /// Optional human-readable reason surfaced when `accepted == false`.
    pub error: Option<String>,
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
    /// [`crate::SupervisorServices::create_task_run`].  Phase 3 wire
    /// surface; dead until Phase 4 actually drives this from the worker.
    CreateTaskRun {
        params: SerializableCreateTaskRunParams,
    },
    /// [`crate::SupervisorServices::update_task_run_status`].
    UpdateTaskRunStatus { run_id: String, status: TaskRunStatus },
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
    PublishSessionMessage {
        session_id: String,
        task_id: String,
        agent_type: String,
        message: serde_json::Value,
    },
    /// [`crate::SupervisorServices::get_environment_config`].  Phase 6d —
    /// project environment-config extraction.
    GetEnvironmentConfig { project_id: String },
    /// [`crate::SupervisorServices::invoke_llm`].  Phase 6a-redux —
    /// host-side LLM invocation. The worker (Phase 7) will use this to keep
    /// vault credentials off the worker side.
    InvokeLlm {
        model_id: String,
        conversation: djinn_provider::message::Conversation,
        tools: Vec<serde_json::Value>,
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
    },
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
    GetEnvironmentConfig(Result<EnvironmentConfig, String>),
    /// Phase 6a-redux — host-side LLM invocation response.
    InvokeLlm(Result<djinn_provider::provider::LlmResponse, String>),
    /// Phase 6e — session status update response.
    UpdateSessionStatus(Result<(), String>),
    /// Transport-level failure — not produced by normal operation.
    Err(String),
}

#[cfg(test)]
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
            verification_failure_count: 0,
            total_reopen_count: 0,
            total_verification_failure_count: 0,
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
            unresolved_blocker_count: 0,
        }
    }

    fn fake_spec() -> TaskRunSpec {
        TaskRunSpec {
            task_id: "t1".into(),
            project_id: "p1".into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/t1".into(),
            flow: SupervisorFlow::NewTask,
            model_id_per_role: HashMap::new(),
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
        let resp =
            ServiceRpcResponse::ExecuteStage(Err(StageError::Setup("no such role".into())));
        let f = Frame {
            correlation_id: 2,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::ExecuteStage(Err(
                StageError::Setup(msg),
            ))) => {
                assert_eq!(msg, "no such role");
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
    fn create_task_run_request_roundtrip() {
        let params = SerializableCreateTaskRunParams {
            id: "run-create-1".into(),
            project_id: "p1".into(),
            task_id: "t1".into(),
            trigger_type: "new_task".into(),
            status: Some("running".into()),
            workspace_path: Some("/workspace".into()),
            mirror_ref: Some("refs/mirror/p1".into()),
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
        let resp =
            ServiceRpcResponse::GetProviderBaseUrl(Ok("https://api.anthropic.com".into()));
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
        let resp = ServiceRpcResponse::PickAnyDefaultModel(Ok(Some(
            "openai/gpt-4o-mini".into(),
        )));
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
            task_run_id: Some("run-1".into()),
            title: None,
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
        let msg = serde_json::json!({ "role": "assistant", "content": "hi" });
        let f = Frame {
            correlation_id: 32,
            payload: FramePayload::Rpc(ServiceRpcRequest::PublishSessionMessage {
                session_id: "s1".into(),
                task_id: "t1".into(),
                agent_type: "worker".into(),
                message: msg.clone(),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
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
                assert_eq!(message, msg);
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
        let cfg = EnvironmentConfig::empty();
        let resp = ServiceRpcResponse::GetEnvironmentConfig(Ok(cfg));
        let f = Frame {
            correlation_id: 33,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert!(matches!(
            back.payload,
            FramePayload::RpcReply(ServiceRpcResponse::GetEnvironmentConfig(Ok(_)))
        ));
    }

    #[test]
    fn invoke_llm_request_roundtrip() {
        use djinn_provider::message::{Conversation, Message};
        use djinn_provider::provider::ToolChoice;
        let mut conv = Conversation::new();
        conv.push(Message::user("hello"));
        let f = Frame {
            correlation_id: 41,
            payload: FramePayload::Rpc(ServiceRpcRequest::InvokeLlm {
                model_id: "anthropic/claude-opus-4-7".into(),
                conversation: conv,
                tools: vec![serde_json::json!({"name": "noop"})],
                tool_choice: Some(ToolChoice::Auto),
            }),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::Rpc(ServiceRpcRequest::InvokeLlm {
                model_id,
                conversation,
                tools,
                tool_choice,
            }) => {
                assert_eq!(model_id, "anthropic/claude-opus-4-7");
                assert_eq!(conversation.len(), 1);
                assert_eq!(tools.len(), 1);
                assert_eq!(tool_choice, Some(ToolChoice::Auto));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn invoke_llm_reply_roundtrip() {
        use djinn_core::message::ContentBlock;
        use djinn_provider::provider::{LlmResponse, TokenUsage};
        let resp = ServiceRpcResponse::InvokeLlm(Ok(LlmResponse {
            content: vec![ContentBlock::text("hi back")],
            thinking: String::new(),
            usage: TokenUsage {
                input: 12,
                output: 7,
            },
        }));
        let f = Frame {
            correlation_id: 41,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        match back.payload {
            FramePayload::RpcReply(ServiceRpcResponse::InvokeLlm(Ok(r))) => {
                assert_eq!(r.usage.input, 12);
                assert_eq!(r.usage.output, 7);
                assert_eq!(r.content.len(), 1);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn update_session_status_request_roundtrip() {
        let f = Frame {
            correlation_id: 42,
            payload: FramePayload::Rpc(ServiceRpcRequest::UpdateSessionStatus {
                session_id: "s1".into(),
                status: SessionStatus::Completed,
                tokens_in: 1234,
                tokens_out: 567,
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
            }) => {
                assert_eq!(session_id, "s1");
                assert_eq!(status, SessionStatus::Completed);
                assert_eq!(tokens_in, 1234);
                assert_eq!(tokens_out, 567);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn update_session_status_reply_roundtrip() {
        let resp = ServiceRpcResponse::UpdateSessionStatus(Ok(()));
        let f = Frame {
            correlation_id: 42,
            payload: FramePayload::RpcReply(resp),
        };
        let bytes = bincode::serialize(&f).unwrap();
        let back: Frame = bincode::deserialize(&bytes).unwrap();
        assert!(matches!(
            back.payload,
            FramePayload::RpcReply(ServiceRpcResponse::UpdateSessionStatus(Ok(())))
        ));
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
}
