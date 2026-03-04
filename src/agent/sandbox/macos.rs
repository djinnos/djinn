// macOS Seatbelt (sandbox-exec) backend.
//
// Full implementation deferred to wave:2 tasks. This stub satisfies the
// module declaration in mod.rs and allows compilation.

use std::path::Path;

use anyhow::Result;

use super::Sandbox;

/// Seatbelt (sandbox-exec) based filesystem sandbox for macOS.
///
/// Applies a per-invocation dynamic policy that permits read-everywhere and
/// write only to the task worktree and /tmp + /var/tmp. Full implementation
/// pending.
pub struct SeatbeltSandbox;

impl Sandbox for SeatbeltSandbox {
    fn apply(&self, _worktree_path: &Path, _cmd: &mut tokio::process::Command) -> Result<()> {
        // TODO(wave:2): generate and apply sandbox-exec policy
        Ok(())
    }
}
