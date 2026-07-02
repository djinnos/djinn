//! Duplex event/request channel between the coordinator and the in-container
//! supervisor.
//!
//! Phase 2 PR 1 — shape-only definitions.  The wire codec (length-prefixed
//! bincode frames) and the matching accept-loop live in `wire.rs` /
//! `local_docker.rs` in later PRs.  For now [`BiStream`] is a pair of
//! in-memory MPSC channels so [`crate::TestRuntime`] can produce one without
//! any IPC machinery.

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::spec::{RoleKind, TaskRunReport};

/// Events flowing upstream from the worker to the coordinator.
///
/// Kept intentionally minimal in PR 1 — the full vocabulary (tool call
/// round-trips, RPC requests, progress heartbeats) lands alongside the RPC
/// wire codec in PR 5.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamEvent {
    /// Partial assistant-message delta from the reply loop — typically a
    /// token chunk being forwarded to the UI's live view.
    AssistantDelta { session_id: String, text: String },
    /// A tool invocation the worker is starting (the full tool_use/tool_result
    /// round-trip is multiplexed over the same socket via `StreamFrame`).
    ToolCall {
        session_id: String,
        tool_name: String,
    },
    /// Structured payload the worker's `finalize_tool` surfaced — e.g. the
    /// planner's plan JSON, the reviewer's decision, the worker's patch
    /// summary.  Opaque bytes at this layer; the coordinator decodes.
    FinalizePayload {
        session_id: String,
        tool_name: String,
        payload: Vec<u8>,
    },
    /// One stage finished — advances the supervisor's role sequence.
    StageOutcome { role: RoleKind, outcome_tag: String },
    /// Coarse stage-init progress marker emitted while the run is setting up,
    /// BEFORE the first reply-loop session exists. Lets the host name the
    /// in-pod step a hung run is stuck on (workspace attach, cargo seed,
    /// context build, ...) and detect when the first turn is reached (the
    /// [`STAGE_STEP_FIRST_TURN`] marker). Diagnostic-only: dropped by the
    /// normal report drain.
    StageStep { step: String },
    /// Terminal: the whole task-run finished.  Always the last frame.
    Report(TaskRunReport),
}

/// Stage-init step markers emitted as [`StreamEvent::StageStep`] /
/// [`crate::WorkerEvent::StageStep`] so the host can name the step a
/// pre-session hang is stuck on. The values are stable wire strings shared by
/// the worker (emitter) and the host (pre-session liveness deadline).
pub mod stage_step {
    /// Attaching/cloning the ephemeral workspace from the mirror.
    pub const WORKSPACE_ATTACH: &str = "workspace_attach";
    /// Seeding the per-run private cargo target dir from the base snapshot.
    pub const CARGO_SEED: &str = "cargo_seed";
    /// Resolving model/credentials, MCP, skills, and assembling the prompt.
    pub const CONTEXT_BUILD: &str = "context_build";
    /// Creating the session row (immediately precedes the reply loop).
    pub const SESSION_CREATE: &str = "session_create";
    /// The reply loop has started — the first provider turn is reached. Seeing
    /// this (or any reply-loop event, or a `sessions` row) disarms the
    /// host-side pre-session liveness deadline.
    pub const FIRST_TURN: &str = "reply_loop";
}

/// Marker step signalling the first reply-loop turn has been reached. Re-export
/// of [`stage_step::FIRST_TURN`] for the host's deadline check.
pub const STAGE_STEP_FIRST_TURN: &str = stage_step::FIRST_TURN;

/// Requests flowing downstream from the coordinator to the worker.
///
/// Same note as [`StreamEvent`] — this is a minimal shape for PR 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamFrame {
    /// Correlated RPC reply for an `mcp_tool_call` / `task_get` / … the
    /// worker initiated (wire codec supplies the correlation-id envelope).
    RpcResponse {
        correlation_id: u64,
        payload: Vec<u8>,
    },
    /// Coordinator wants the task-run cancelled — graceful stop, flush
    /// outstanding events, then exit.
    Cancel,
}

/// Duplex byte-pipe between the coordinator and the in-container supervisor.
///
/// Phase 2 PR 1 — an in-memory MPSC pair.  In PR 5 the same struct shape
/// will be returned by a Unix-socket-backed constructor that owns the
/// `tokio_util::codec::Framed<UnixStream, LengthDelimitedCodec>` and spawns
/// a codec task behind each channel.
pub struct BiStream {
    pub events_rx: mpsc::Receiver<StreamEvent>,
    pub requests_tx: mpsc::Sender<StreamFrame>,
}

impl BiStream {
    /// Construct a paired event/request channel for in-process testing.
    ///
    /// Returns `(BiStream, events_tx, requests_rx)` — the returned sender /
    /// receiver are the other end of the pipes so a test harness can feed
    /// events into `events_rx` and observe the requests the consumer sent.
    pub fn new_in_memory(
        buffer: usize,
    ) -> (Self, mpsc::Sender<StreamEvent>, mpsc::Receiver<StreamFrame>) {
        let (events_tx, events_rx) = mpsc::channel(buffer);
        let (requests_tx, requests_rx) = mpsc::channel(buffer);
        (
            Self {
                events_rx,
                requests_tx,
            },
            events_tx,
            requests_rx,
        )
    }
}
