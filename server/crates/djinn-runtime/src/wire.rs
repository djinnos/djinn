//! Wire primitives shared by the coordinator (launcher side) and the
//! in-container supervisor (worker side).
//!
//! Phase 2 PR 5 of `/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md`.
//!
//! The narrow set of types in this module is intentionally **independent of
//! `djinn-supervisor`** — the concrete `ServiceRpcRequest` / `ServiceRpcResponse`
//! envelope lives in `djinn-supervisor::services::wire` because those variants
//! reference supervisor types (`StageOutcome`, `StageError`, `TaskRunSpec`).
//! Keeping this layer upstream of the supervisor crate lets `djinn-runtime`
//! stay at the bottom of the dependency tree where `djinn-agent-worker` can
//! depend on it without circularity.
//!
//! What lives here today:
//!
//! - [`WorkspaceRef`] — serializable description of a bind-mounted
//!   `/workspace`; the worker rehydrates it into a real
//!   `djinn_workspace::Workspace` via `Workspace::attach_existing`.
//! - [`ControlMsg`] — downstream control signals (cancel / shutdown).
//! - [`WorkerEvent`] — upstream event enum carrying out-of-band signals the
//!   worker emits on the shared frame channel (currently the terminal
//!   [`TaskRunReport`]; future variants cover assistant deltas, tool calls,
//!   stage outcomes, heartbeats).
//! - length-prefixed frame helpers ([`read_frame`], [`write_frame`],
//!   [`MAX_FRAME_BYTES`]).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// Re-export `TaskRunReport` at the `wire` module root so transport code can
// spell it as `djinn_runtime::wire::TaskRunReport` alongside the other wire
// primitives.  The canonical definition stays in `crate::spec`.
pub use crate::spec::TaskRunReport;

/// Maximum frame body size the wire will accept.
///
/// 16 MiB — far larger than any realistic RPC payload but small enough to
/// protect both sides from an OOM when the peer sends a garbage length
/// header.  Matches the worker's pre-PR-5 helper so existing tests keep the
/// same limit.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Serializable description of a workspace the runtime already materialised.
///
/// In `LocalDockerRuntime` the host clones the mirror into a tempdir and
/// bind-mounts it at `/workspace` inside the container; the worker calls
/// [`djinn_workspace::Workspace::attach_existing`] with `path` + `branch` to
/// rehydrate a `Workspace` wrapper without re-cloning.  `owned_by_runtime`
/// documents that the runtime (not the worker) is responsible for lifecycle
/// cleanup — the worker drops the `Workspace::Attached` variant as a no-op.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceRef {
    pub path: PathBuf,
    pub branch: String,
    pub owned_by_runtime: bool,
}

/// Downstream control message from the launcher to the worker.
///
/// Sent as a plain payload variant on the duplex frame channel — no
/// correlation-id coupling because the worker reacts to these out-of-band of
/// any in-flight RPC.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ControlMsg {
    /// Flip the worker's cancellation token.  The supervisor flushes the
    /// current stage, emits the terminal report, and exits.
    Cancel,
    /// Fatal shutdown — drop everything, no report expected.  Reserved for
    /// future use; PR 5 never sends this.
    Shutdown,
}

/// Upstream event produced by the worker.
///
/// Carried as a [`FramePayload::Event`] on the shared bincode frame channel
/// (see `djinn-supervisor::services::wire`).  Variants are intentionally
/// additive: the [`WorkerEvent::Placeholder`] slot pins the enum's on-wire
/// layout against the pre-Phase-2.1 snapshots, and [`WorkerEvent::TerminalReport`]
/// is the real payload the worker emits on run completion.  Subsequent PRs
/// add assistant deltas, tool calls, stage outcomes, and heartbeats.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WorkerEvent {
    /// Reserved — preserves the pre-Phase-2.1 enum layout.  Never emitted
    /// by the worker in normal operation; retained so the wire shape stays
    /// back-compatible against anything that persisted a serialized
    /// [`WorkerEvent`] during Phase 2 PR 5.
    Placeholder,
    /// Terminal [`TaskRunReport`] the worker emits when `drive_placeholder`
    /// (and, in later PRs, the real `TaskRunSupervisor::run`) finishes.
    /// The launcher's [`KubernetesRuntime::teardown`] drains the pending
    /// connection's event stream looking for this variant to extract the
    /// real terminal report instead of synthesising one from Job status.
    TerminalReport(TaskRunReport),
}

/// Write `payload` as a length-prefixed bincode frame.
///
/// The header is a `u32` big-endian byte count followed by the serialized
/// body.  Flushes the writer before returning so the frame reaches the peer
/// without buffering surprises.  Returns the body length in bytes.
pub async fn write_frame<W, T>(writer: &mut W, payload: &T) -> Result<usize>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = bincode::serialize(payload).context("bincode serialize frame")?;
    let len: u32 = body
        .len()
        .try_into()
        .context("frame body exceeds u32::MAX")?;
    if len > MAX_FRAME_BYTES {
        anyhow::bail!("frame body {} bytes exceeds MAX_FRAME_BYTES", len);
    }
    writer
        .write_all(&len.to_be_bytes())
        .await
        .context("write frame length")?;
    writer.write_all(&body).await.context("write frame body")?;
    writer.flush().await.context("flush frame")?;
    Ok(body.len())
}

/// Read a single length-prefixed bincode frame from `reader`.
pub async fn read_frame<R, T>(reader: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .context("read frame length")?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        anyhow::bail!("incoming frame {} bytes exceeds MAX_FRAME_BYTES", len);
    }
    let mut body = vec![0u8; len as usize];
    reader
        .read_exact(&mut body)
        .await
        .context("read frame body")?;
    bincode::deserialize(&body).context("bincode deserialize frame")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn workspace_ref_roundtrip() {
        let r = WorkspaceRef {
            path: PathBuf::from("/workspace"),
            branch: "djinn/t1".into(),
            owned_by_runtime: true,
        };
        let bytes = bincode::serialize(&r).unwrap();
        let back: WorkspaceRef = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back.path, r.path);
        assert_eq!(back.branch, r.branch);
        assert_eq!(back.owned_by_runtime, r.owned_by_runtime);
    }

    #[tokio::test]
    async fn frame_roundtrip_over_duplex() {
        let (mut a, mut b) = duplex(1024);
        let send = ControlMsg::Cancel;
        tokio::try_join!(
            async {
                write_frame(&mut a, &send).await?;
                Ok::<_, anyhow::Error>(())
            },
            async {
                let back: ControlMsg = read_frame(&mut b).await?;
                assert!(matches!(back, ControlMsg::Cancel));
                Ok::<_, anyhow::Error>(())
            }
        )
        .expect("roundtrip");
    }

    #[tokio::test]
    async fn oversized_length_is_rejected() {
        let (mut a, mut b) = duplex(64);
        let oversize = MAX_FRAME_BYTES + 1;
        a.write_all(&oversize.to_be_bytes()).await.unwrap();
        let err: Result<Vec<u8>> = read_frame(&mut b).await;
        assert!(err.is_err(), "oversized frame should error");
    }

    /// The `WorkerEvent::TerminalReport(TaskRunReport)` variant survives a
    /// bincode round-trip — proves the new wire variant is encode/decode
    /// parity and the re-exported `TaskRunReport` is compatible with the
    /// serde derives on `spec.rs`.
    #[tokio::test]
    async fn terminal_report_event_bincode_roundtrip() {
        use crate::spec::{RoleKind, TaskRunOutcome};

        let report = TaskRunReport {
            task_run_id: "run-terminal-1".into(),
            outcome: TaskRunOutcome::PrOpened {
                url: "https://github.com/o/r/pull/7".into(),
                sha: "cafebabe".into(),
            },
            stages_completed: vec![RoleKind::Planner, RoleKind::Worker, RoleKind::Reviewer],
        };
        let event = WorkerEvent::TerminalReport(report);

        let bytes = bincode::serialize(&event).expect("serialize TerminalReport");
        let back: WorkerEvent = bincode::deserialize(&bytes).expect("deserialize TerminalReport");

        match back {
            WorkerEvent::TerminalReport(r) => {
                assert_eq!(r.task_run_id, "run-terminal-1");
                assert_eq!(
                    r.stages_completed,
                    vec![RoleKind::Planner, RoleKind::Worker, RoleKind::Reviewer]
                );
                match r.outcome {
                    TaskRunOutcome::PrOpened { url, sha } => {
                        assert_eq!(url, "https://github.com/o/r/pull/7");
                        assert_eq!(sha, "cafebabe");
                    }
                    other => panic!("unexpected outcome: {other:?}"),
                }
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// The legacy `Placeholder` variant still round-trips — guards against
    /// accidentally reshuffling the enum's variant indices in a way that
    /// would break any persisted frames from Phase 2 PR 5.
    #[tokio::test]
    async fn placeholder_event_bincode_roundtrip() {
        let bytes = bincode::serialize(&WorkerEvent::Placeholder).expect("serialize Placeholder");
        let back: WorkerEvent = bincode::deserialize(&bytes).expect("deserialize Placeholder");
        assert!(matches!(back, WorkerEvent::Placeholder));
    }
}
