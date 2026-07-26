//! What a [`CommandSpec`](crate::CommandSpec) path may be, and what that
//! guarantee is worth.
//!
//! Split out of `lib.rs` so the decision recorded below — the one the seventh
//! goxi launcher blocker forced — has a place to live next to its tests rather
//! than a comment wedged between two unrelated types.

/// Path hygiene for the two paths a [`CommandSpec`] carries.
///
/// # What this is, and what it is deliberately NOT (task goxi, seventh blocker)
///
/// The load-bearing half is **shape**: absolute, normalized, NUL-free. That is
/// not tidiness. `spawn.rs` hands `program` straight to `execve(2)`, which
/// performs no `PATH` search, so a bare name can only ever `ENOENT`; and a
/// *relative* program or cwd would be resolved against a directory the caller
/// influences. Both are refused here, before a leaf or a child exists.
///
/// The directory prefix list is the weaker half, and it must not be mistaken
/// for a code-provenance control, because it is not one and cannot be made into
/// one:
///
/// * `/workspace/` is on it, and `/workspace` is the tree the agent writes. A
///   caller can put a binary there and name it. That alone settles it.
/// * Decisively, the *only* program a brokered command ever names is a shell.
///   `extension::handlers::workspace` builds `bash -lc <command>`, and `bash`
///   then resolves `<command>` itself against a `PATH` that is mostly outside
///   this list. Measured in the rendered task image on the production node:
///
///   ```text
///   PATH=/opt/djinn/bin:/usr/local/cargo/bin:/opt/node/bin:/usr/local/go/bin:
///        /go/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
///   cargo -> /usr/local/cargo/bin/cargo      node -> /opt/node/bin/node
///   ```
///
///   So a brokered `cargo build` already executes a binary from a directory
///   this list does not name, and always did. What actually confines a brokered
///   child is `child::prepare_child` (uid 1001, no capabilities, `no_new_privs`,
///   seccomp), its cgroup leaf, its mount namespace, and the closed environment
///   allow-list — none of which this function participates in.
///
/// # Why it is nevertheless left exactly as it is
///
/// Widening it to `/usr/local/bin/` or `/opt/djinn/bin/` — the two directories
/// the seventh blocker's investigation found it rejects — would buy nothing and
/// would advertise a guarantee that does not exist. Measured in the same image:
/// `/usr/local/bin` is **empty**, and `/opt/djinn/bin` holds exactly
/// `djinn-agent-worker` and `djinn-cgroup-launcher`, neither of which is ever a
/// brokered child. There is no caller for either entry.
///
/// The real hazard of a narrow list is the one that produced this blocker: a
/// caller trips it and fails closed with a coarse error. That is fixed where it
/// belongs, in `djinn_agent::process_broker::resolve_program`, by making the
/// `Command` -> `CommandSpec` conversion honour the `PATH` search `Command`
/// documents — so a caller who names a program the ordinary way now gets the
/// absolute path this function asks for, instead of a refusal. Guessing at
/// directories would have papered over that and left the conversion lossy.
pub fn safe_command_path(path: &str, cwd: bool) -> bool {
    !path.contains('\0')
        && !path.contains("//")
        && !path.split('/').any(|part| part == "..")
        && if cwd {
            path == "/workspace" || path.starts_with("/workspace/")
        } else {
            path.starts_with("/bin/")
                || path.starts_with("/usr/bin/")
                || path.starts_with("/workspace/")
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape half — the half that is load-bearing. `execve(2)` performs no
    /// `PATH` search and resolves nothing, so a bare or relative program can
    /// only ever name the wrong thing or nothing at all.
    #[test]
    fn only_absolute_normalized_paths_are_accepted() {
        for malformed in [
            // The seventh blocker: what `Command::new("bash")` converts to.
            "bash",
            "./bash",
            "../bin/bash",
            "usr/bin/bash",
            "/usr/bin/../etc/passwd",
            "/usr//bin/bash",
            "/bin/ba\0sh",
            "",
        ] {
            assert!(
                !safe_command_path(malformed, false),
                "{malformed:?} is not a shape `execve` can use"
            );
        }
        for path in ["/bin/bash", "/usr/bin/git", "/workspace/tool"] {
            assert!(safe_command_path(path, false), "{path:?} must be accepted");
        }
    }

    /// The cwd half is stricter on purpose: a brokered command runs in the task
    /// workspace and nowhere else, and `spawn.rs` `chdir`s there before `execve`.
    #[test]
    fn a_cwd_must_be_the_workspace_or_beneath_it() {
        for path in ["/workspace", "/workspace/project/worktree"] {
            assert!(safe_command_path(path, true), "{path} must be a valid cwd");
        }
        for path in ["/", "/tmp", "/workspacex", "/bin/bash", "workspace"] {
            assert!(!safe_command_path(path, true), "{path} must not be a cwd");
        }
    }

    /// The decision, pinned. Widening the program prefix list is a change of
    /// posture, not a typo fix — read the module docs before touching this.
    ///
    /// Measured in the rendered task image on the production node:
    /// `/usr/local/bin` is empty, and `/opt/djinn/bin` holds only
    /// `djinn-agent-worker` and `djinn-cgroup-launcher`, neither of which is
    /// ever a brokered child. There is no caller for either entry, and the list
    /// confines nothing anyway — the one program a brokered command names is a
    /// shell, which resolves everything else itself.
    #[test]
    fn the_program_prefix_list_stays_at_bin_usr_bin_and_workspace() {
        for outside in [
            "/usr/local/bin/anything",
            "/opt/djinn/bin/djinn-agent-worker",
            "/usr/local/cargo/bin/cargo",
            "/opt/node/bin/node",
            "/sbin/ip",
            "/cache/go/bin/tool",
        ] {
            assert!(
                !safe_command_path(outside, false),
                "{outside} is deliberately outside the program allow-list"
            );
        }
    }
}
