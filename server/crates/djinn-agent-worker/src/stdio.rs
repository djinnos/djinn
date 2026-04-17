//! Stdin slurp helper.
//!
//! The launcher pipes a bincode-serialized `TaskRunSpec` into the worker's
//! stdin as one atomic blob (no length prefix — stdin closes after the
//! payload, which is how we know the frame is complete).  PR 5's richer
//! IPC wire format will move the spec onto the Unix socket; today stdin
//! keeps the surface area minimal.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Read every byte from `reader` into a buffer and `bincode::deserialize`
/// it as `T`.
pub async fn read_bincode_from_stdin<R, T>(reader: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut buf = Vec::with_capacity(4096);
    reader
        .read_to_end(&mut buf)
        .await
        .context("read stdin to EOF")?;
    bincode::deserialize(&buf).context("bincode deserialize stdin")
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::models::TaskRunTrigger;
    use djinn_runtime::{SupervisorFlow, TaskRunSpec};
    use std::collections::HashMap;
    use tokio::io::duplex;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn spec_roundtrips_through_stdin_slurp() {
        let (mut w, mut r) = duplex(8192);
        let spec = TaskRunSpec {
            task_id: "t1".into(),
            project_id: "p1".into(),
            trigger: TaskRunTrigger::NewTask,
            base_branch: "main".into(),
            task_branch: "djinn/t1".into(),
            flow: SupervisorFlow::Spike,
            model_id_per_role: HashMap::new(),
        };
        let bytes = bincode::serialize(&spec).expect("serialize");

        // Write + close, then read.
        w.write_all(&bytes).await.unwrap();
        drop(w); // EOF triggers `read_to_end` completion.
        let back: TaskRunSpec = read_bincode_from_stdin(&mut r).await.expect("roundtrip");
        assert_eq!(back.task_id, "t1");
        assert_eq!(back.flow, SupervisorFlow::Spike);
    }
}
