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
    /// Terminal: the whole task-run finished.  Always the last frame.
    Report(TaskRunReport),
}

/// Requests flowing downstream from the coordinator to the worker.
///
/// Same note as [`StreamEvent`] — this is a minimal shape for PR 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum StreamFrame {
    /// Correlated RPC reply for an `mcp_tool_call` / `task_get` / … the
    /// worker initiated (wire codec supplies the correlation-id envelope).
    RpcResponse { correlation_id: u64, payload: Vec<u8> },
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
    ) -> (
        Self,
        mpsc::Sender<StreamEvent>,
        mpsc::Receiver<StreamFrame>,
    ) {
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
