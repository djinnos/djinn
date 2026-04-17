//! Bincode-over-unix-socket wire envelope for [`SupervisorServices`].
//!
//! Phase 2 PR 5 of `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
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

use djinn_core::models::Task;
use djinn_runtime::wire::{ControlMsg, WorkerEvent, WorkspaceRef};
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
}

/// Typed request variants — one per trait method on [`crate::SupervisorServices`]
/// except `cancel()`, which is satisfied locally on the worker and does not
/// cross the wire.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
}

/// Typed response variants — one per [`ServiceRpcRequest`] variant.
///
/// `Err(String)` is reserved for transport-level failures (the worker
/// encountered a protocol violation / connection reset / serialization
/// error).  Semantic errors inside a call (e.g. `load_task` returning
/// `Err("task not found")`) travel inside the matching typed variant.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServiceRpcResponse {
    LoadTask(Result<Task, String>),
    ExecuteStage(Result<StageOutcome, StageError>),
    OpenPr(TaskRunOutcome),
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
}
