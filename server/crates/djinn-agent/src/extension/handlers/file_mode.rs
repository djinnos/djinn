//! `set_file_mode`: flip a worktree file's executable bit **as its owner**.
//!
//! # Why a tool exists for something `chmod` already does
//!
//! Inode-metadata operations (`chmod`, `chown`, `utimensat`) are governed by
//! **ownership alone**. No mode bit, setgid bit, ACL, or group membership can
//! delegate them, and the kernel returns `EPERM` to a non-owner even when the
//! requested mode is byte-identical to the current one.
//!
//! Files written through `write` / `edit` / `apply_patch` are created by the
//! worker process at uid 1000. The agent's `shell` runs as the launcher-spawned
//! child at uid 1001 (`CHILD_UID`). So `chmod +x` from the shell fails with
//! `Operation not permitted` on every file the agent authored through a tool —
//! and there is no shell-side workaround, because the shell is not and cannot
//! become the owner.
//!
//! This handler runs in the worker process, which *is* the owner, so its
//! `set_permissions` succeeds. Making disk agree is the root fix: every
//! subsequent `git add` reads the mode from disk, so the bit reaches the commit
//! for free, and an in-pod test can actually run `./script.sh`.
//!
//! Production task `tv9g` burned nine sessions and four review cycles for want
//! of this: the agent tried `chmod` (EPERM), fell back to
//! `git update-index --chmod=+x` (correct, but index-only), and the commit path
//! then dropped the staged mode. `djinn_git::index_mode` fixes that second half;
//! this module fixes the first.
//!
//! # Why the argument is a boolean and not a mode
//!
//! A numeric mode would put `setuid`, `setgid`, and the sticky bit inside the
//! agent's reach, leaving "reject those" as a validation rule that has to hold
//! forever against every future caller. A boolean makes them **unrepresentable**
//! instead — the strongest available form of that guarantee, and the reason this
//! tool takes `executable` rather than `mode`.
//!
//! The one case a boolean cannot express is a file that *already* carries
//! setuid/setgid/sticky. Rather than silently preserving those bits (shipping an
//! agent-triggered setuid binary) or silently clearing them (a mode change
//! nobody asked for), the handler refuses the path outright and says so.

use std::os::unix::fs::PermissionsExt;

use serde::Deserialize;

use super::*;

/// `rwx` for user, group, and other.
const PERMISSION_BITS: u32 = 0o777;

/// The executable bits, one per class.
const EXECUTE_BITS: u32 = 0o111;

/// The read bits, one per class. `+x` mirrors these so a `600` file becomes
/// `700` rather than a world-executable `711`.
const READ_BITS: u32 = 0o444;

/// setuid | setgid | sticky. Never set by this handler, and its presence on the
/// target is a hard refusal rather than something to preserve or clear silently.
const SPECIAL_BITS: u32 = 0o7000;

#[derive(Debug, Deserialize)]
struct SetFileModeParams {
    path: String,
    executable: bool,
}

/// Compute the new permission bits for an executable-bit flip.
///
/// `+x` sets the execute bit for each class that can already read the file;
/// `-x` clears all three. Read/write bits are otherwise preserved, and the
/// special bits are never introduced.
///
/// Extracted so the bit arithmetic is testable without a filesystem — the
/// interesting cases (`600`, `640`, `444`) are awkward to stage as real files
/// under an arbitrary umask.
pub(crate) fn next_permission_bits(current: u32, executable: bool) -> u32 {
    let base = current & PERMISSION_BITS;
    if executable {
        base | ((base & READ_BITS) >> 2)
    } else {
        base & !EXECUTE_BITS
    }
}

/// `state` is deliberately absent: unlike `write`/`edit` this changes no bytes,
/// so there is no read-record to invalidate, no LSP file to touch, and no
/// related-files enrichment to attach. Taking an `AgentContext` it never reads
/// would imply otherwise.
pub(crate) async fn call_set_file_mode(
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: SetFileModeParams = parse_args(arguments)?;
    let path = resolve_path(&p.path, worktree_path);

    // Canonicalizing containment check: a symlink is resolved before the prefix
    // test, so one pointing out of the worktree is rejected rather than followed
    // by the `set_permissions` below (which follows symlinks too).
    ensure_path_within_worktree(&path, worktree_path)?;

    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;

    // Directories have their own meaning for `x` (traversal, not execution) and
    // are not what any caller of this tool wants; special files are not ours to
    // touch. `metadata` follows symlinks, so this also rejects a symlink to a
    // directory.
    if !metadata.is_file() {
        return Err(format!(
            "set_file_mode only applies to regular files; {} is not one",
            path.display()
        ));
    }

    let current = metadata.permissions().mode();

    // Refusing beats preserving (which would ship an agent-triggered setuid
    // binary) and beats clearing (an unrequested mode change on a file whose
    // special bits someone set deliberately).
    if current & SPECIAL_BITS != 0 {
        return Err(format!(
            "refusing to change the mode of {}: it carries setuid/setgid/sticky bits ({:04o}). \
             Adjust those deliberately outside this tool.",
            path.display(),
            current & SPECIAL_BITS
        ));
    }

    let next = next_permission_bits(current, p.executable);
    let already = (current & PERMISSION_BITS) == next;

    if !already {
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(next))
            .await
            .map_err(|e| {
                format!(
                    "chmod {:04o} {} failed: {e}. The worker process owns files it wrote; \
                     a path created outside it may be owned by another uid.",
                    next,
                    path.display()
                )
            })?;
    }

    // No `file_time` invalidation and no LSP touch: the bytes did not change, so
    // the model's in-context view of the content is still accurate. Forcing a
    // re-read here would make `set_file_mode` a surprising cache buster.

    let git_mode = if p.executable {
        djinn_git::MODE_EXECUTABLE
    } else {
        djinn_git::MODE_REGULAR
    };

    tracing::info!(
        path = %path.display(),
        executable = p.executable,
        previous_mode = format!("{:04o}", current & PERMISSION_BITS),
        new_mode = format!("{next:04o}"),
        unchanged = already,
        "set_file_mode: applied executable bit"
    );

    Ok(serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
        "executable": p.executable,
        "mode": format!("{next:04o}"),
        "previous_mode": format!("{:04o}", current & PERMISSION_BITS),
        "unchanged": already,
        // Surfaced so the agent can assert the thing that actually matters and
        // stop at the right evidence: `git ls-tree HEAD` after committing, not
        // `git ls-files -s`, which reads green on an index entry a restage can
        // still discard.
        "expected_git_mode": git_mode,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adding_x_mirrors_the_read_bits() {
        assert_eq!(next_permission_bits(0o644, true), 0o755);
        assert_eq!(next_permission_bits(0o600, true), 0o700);
        assert_eq!(next_permission_bits(0o640, true), 0o750);
        assert_eq!(next_permission_bits(0o444, true), 0o555);
    }

    /// A world-writable-but-unreadable file must not gain an execute bit for a
    /// class that cannot read it.
    #[test]
    fn adding_x_does_not_invent_execute_for_an_unreadable_class() {
        assert_eq!(next_permission_bits(0o600, true) & 0o011, 0);
    }

    #[test]
    fn removing_x_clears_every_class() {
        assert_eq!(next_permission_bits(0o755, false), 0o644);
        assert_eq!(next_permission_bits(0o700, false), 0o600);
        assert_eq!(next_permission_bits(0o111, false), 0o000);
    }

    #[test]
    fn is_idempotent_in_both_directions() {
        let up = next_permission_bits(0o644, true);
        assert_eq!(next_permission_bits(up, true), up);
        let down = next_permission_bits(0o755, false);
        assert_eq!(next_permission_bits(down, false), down);
    }

    /// The bit arithmetic must never introduce setuid/setgid/sticky, whatever it
    /// is handed. The handler refuses such files earlier; this pins the
    /// arithmetic itself so a future numeric-mode parameter cannot smuggle them
    /// in through this function.
    #[test]
    fn never_produces_special_bits() {
        for mode in 0..=0o777u32 {
            assert_eq!(next_permission_bits(mode, true) & SPECIAL_BITS, 0);
            assert_eq!(next_permission_bits(mode, false) & SPECIAL_BITS, 0);
        }
        // Even fed a mode that already carries them, the returned bits are
        // masked to the permission bits alone.
        assert_eq!(next_permission_bits(0o4755, true) & SPECIAL_BITS, 0);
        assert_eq!(next_permission_bits(0o2644, true) & SPECIAL_BITS, 0);
        assert_eq!(next_permission_bits(0o1644, true) & SPECIAL_BITS, 0);
    }

    /// git records exactly two modes for a regular file, and the response must
    /// name the one the agent should expect at `ls-tree`.
    #[test]
    fn the_advertised_git_modes_are_the_only_two_git_records() {
        assert_eq!(djinn_git::MODE_EXECUTABLE, "100755");
        assert_eq!(djinn_git::MODE_REGULAR, "100644");
    }
}
