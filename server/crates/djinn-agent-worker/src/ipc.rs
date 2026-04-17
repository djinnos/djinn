//! Length-prefixed bincode frame helpers over a `UnixStream`.
//!
//! Phase 2 PR 4 — placeholder surface.  PR 5 replaces this with the richer
//! `ServiceRpcRequest` / `ServiceRpcResponse` / `StreamEvent` codec sketched
//! in the blueprint (`/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md` §3).
//!
//! Today we only need enough to prove the wire plumbing works end-to-end:
//!   * dial the launcher's Unix socket,
//!   * ship a single arbitrary bincode-serializable payload,
//!   * read a single response back.
//!
//! Frames are `u32` big-endian length + bincode body.  This matches the
//! framing `tokio_util::codec::LengthDelimitedCodec` produces by default, so
//! PR 5 can swap these helpers for a `Framed<UnixStream, ...>` pipeline
//! without breaking the wire format.
//!
//! The helpers are intentionally `async fn`s on raw halves rather than
//! methods on a wrapper struct — that lets `main.rs` split a `UnixStream`
//! into read/write halves and drive them from separate tasks as the codec
//! gets richer.

use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Maximum frame body size we'll accept from the wire.
///
/// 16 MiB is vastly more than any realistic RPC payload (the heaviest today
/// is the canonical graph snapshot, which we deliberately do NOT ship over
/// this channel — see blueprint §3).  Capping keeps a malformed length
/// header from causing an OOM on either side.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Write a single length-prefixed bincode frame to `writer`.
///
/// Returns the number of body bytes written (not including the 4-byte
/// length header).
pub async fn write_frame<W, T>(writer: &mut W, payload: &T) -> Result<usize>
where
    W: AsyncWriteExt + Unpin,
    T: Serialize,
{
    let body = bincode::serialize(payload).context("bincode serialize")?;
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
    R: AsyncReadExt + Unpin,
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
    bincode::deserialize(&body).context("bincode deserialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn frame_roundtrip_over_duplex() {
        let (mut a, mut b) = duplex(1024);
        let send: Vec<String> = vec!["hello".into(), "world".into()];
        tokio::try_join!(
            async {
                write_frame(&mut a, &send).await?;
                Ok::<_, anyhow::Error>(())
            },
            async {
                let back: Vec<String> = read_frame(&mut b).await?;
                assert_eq!(back, vec!["hello".to_string(), "world".to_string()]);
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
        assert!(err.is_err(), "expected oversized frame to error");
    }
}
