// Linux Landlock sandbox backend.
//
// Full implementation deferred to wave:2 tasks. This stub satisfies the
// module declaration in mod.rs and allows compilation.

use std::path::Path;

use anyhow::Result;

use super::Sandbox;

/// Landlock-based filesystem sandbox for Linux ≥ 5.13.
///
/// Restricts the agent child process to read-everywhere, write only to the
/// task worktree and /tmp + /var/tmp. Full implementation pending.
pub struct LandlockSandbox;

impl Sandbox for LandlockSandbox {
    fn apply(&self, _worktree_path: &Path, _cmd: &mut tokio::process::Command) -> Result<()> {
        // TODO(wave:2): set up Landlock Ruleset with worktree write access
        Ok(())
    }
}
