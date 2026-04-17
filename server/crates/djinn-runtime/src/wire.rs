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
//! - [`WorkerEvent`] — placeholder upstream event enum; no producers in this
//!   PR, it pins the wire shape so later PRs don't have to renumber variants.
//! - length-prefixed frame helpers ([`read_frame`], [`write_frame`],
//!   [`MAX_FRAME_BYTES`]).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

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
/// Placeholder in PR 5 — no producers exist yet.  The enum is present so
/// the wire envelope has a stable variant layout before PR 6/7 starts
/// emitting real events.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WorkerEvent {
    /// Reserved — swapped for real variants (assistant deltas, tool calls,
    /// stage outcomes, progress heartbeats) in later PRs.
    Placeholder,
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
}
