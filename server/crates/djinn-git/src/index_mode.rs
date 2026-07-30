//! Preserve **index-staged executable-mode intent** across a `reset` + `add`
//! restage cycle.
//!
//! # The defect this closes
//!
//! A file mode lives only in the git *index entry*, never in the blob. Content
//! survives a restage because it is on disk; a mode does not. Both of djinn's
//! commit paths restage before committing —
//! `djinn-agent-worker`'s checkpoint does `git reset --mixed HEAD --` then a
//! targeted `git add <path>` per safety-approved file, and `djinn-workspace`'s
//! post-worker auto-commit does the same (plus `git add -A` on the merge-parent
//! path). Every one of those `add`s re-reads the mode **from disk**.
//!
//! So an agent that stages `100755` with `git update-index --chmod=+x` — the
//! only tool it has when the worktree rejects `chmod` — watches the bit
//! silently revert to `100644` at commit time. That is not hypothetical:
//! production task `tv9g` burned nine sessions and four review cycles on it.
//! The worker's fallback was correct; the commit path threw it away.
//!
//! # Why the worktree rejects `chmod` in the first place
//!
//! Inode-metadata operations (`chmod`, `chown`, `utimensat`) are governed by
//! **ownership alone** — no mode bit, setgid bit, ACL or group membership can
//! delegate them, and the kernel returns `EPERM` to a non-owner even when the
//! requested mode is byte-identical to the current one. Files written through
//! the `write`/`edit` tools are created by the worker process (uid 1000); the
//! agent's shell runs as the launcher-spawned child (uid 1001). So the shell
//! cannot `chmod` what the worker wrote, and `git update-index --chmod=+x` is
//! its only route to a `100755` index entry.
//!
//! The companion fix is the worker-side `set_file_mode` tool, which chmods as
//! the owner and makes disk agree. This module is the belt-and-braces half: it
//! makes the index-only route survive to `HEAD` even when no one calls that
//! tool, and even on a mount where `chmod` can never succeed.
//!
//! # Why this module holds no git invocations
//!
//! The two call sites have different git runners (one bare `tokio::process`
//! helper, one `Workspace::run_git` with actor serialization and identity env).
//! Parsing and set arithmetic are the whole of the shared logic, so they live
//! here as pure functions over captured output and stay unit-testable without
//! spawning a process or building a repository. Each caller keeps its own
//! `ls-files` / `update-index` plumbing.
//!
//! # Verification note
//!
//! A test for this behaviour must assert the bit at `git ls-tree HEAD`, never
//! `git ls-files -s`. The index is exactly the thing that reads green on a
//! change the commit path is about to discard — asserting it would reproduce
//! the tv9g reviewer's four rejected cycles as a passing test.

use std::collections::BTreeMap;

/// The git index mode for a regular executable file.
pub const MODE_EXECUTABLE: &str = "100755";

/// The git index mode for a regular non-executable file.
pub const MODE_REGULAR: &str = "100644";

/// Path → "is this index entry an executable regular file", parsed from
/// `git ls-files -s` output.
pub type IndexModes = BTreeMap<String, bool>;

/// Parse `git ls-files -s` output into path → executable-regular-file flag.
///
/// Each line is `<mode> <object> <stage>\t<path>`. Only regular-file modes are
/// recorded: a symlink (`120000`) or gitlink (`160000`) has no executable bit
/// to preserve, and treating one as non-executable would make this module
/// *propose* a `--chmod=+x` that git would reject.
///
/// Unmerged entries (stage 1/2/3) appear more than once per path. They are
/// skipped outright — a conflicted path has no single mode intent to carry, and
/// the restage that follows a conflict resolution is precisely when guessing
/// one would be wrong.
pub fn parse_index_modes(output: &str) -> IndexModes {
    let mut modes: IndexModes = BTreeMap::new();
    let mut conflicted: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for line in output.lines() {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }

        let mut fields = meta.split_whitespace();
        let Some(mode) = fields.next() else {
            continue;
        };
        // `<object>` then `<stage>`.
        let stage = fields.nth(1).unwrap_or("0");

        if stage != "0" {
            conflicted.insert(path.to_string());
            continue;
        }

        match mode {
            MODE_EXECUTABLE => {
                modes.insert(path.to_string(), true);
            }
            MODE_REGULAR => {
                modes.insert(path.to_string(), false);
            }
            // Symlink, gitlink, or anything else: no executable bit of ours.
            _ => {}
        }
    }

    for path in conflicted {
        modes.remove(&path);
    }

    modes
}

/// Path → executable flag, parsed from `git ls-tree -r <rev>` output.
///
/// A tree line is `<mode> <type> <object>\t<path>` — one field more than
/// `git ls-files -s`, and with no stage column — so it needs its own parser.
/// Feeding tree output to [`parse_index_modes`] would read the object hash as a
/// stage number, decide every path is conflicted, and silently return nothing.
pub fn parse_tree_modes(output: &str) -> IndexModes {
    let mut modes: IndexModes = BTreeMap::new();

    for line in output.lines() {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        let Some(mode) = meta.split_whitespace().next() else {
            continue;
        };
        match mode {
            MODE_EXECUTABLE => {
                modes.insert(path.to_string(), true);
            }
            MODE_REGULAR => {
                modes.insert(path.to_string(), false);
            }
            _ => {}
        }
    }

    modes
}

/// Paths whose staged executable bit a restage dropped and which should be
/// re-marked with `git update-index --chmod=+x`.
///
/// `head` is the committed tree's modes ([`parse_tree_modes`]), and it is what
/// separates the two situations that otherwise look identical after the restage:
///
/// - **Staged intent** — `HEAD` has the path at `100644` or not at all, the index
///   had it at `100755`. The executable bit is a change the agent staged, and the
///   restage lost it. Restore it. (This is tv9g.)
/// - **A deliberate `chmod -x`** — `HEAD` already had the path at `100755`, so the
///   index's `100755` was merely inherited and carries no new intent; the *disk*
///   is where the intent lives, and the restage correctly picked it up. Restoring
///   here would silently revert the agent's `chmod -x`.
///
/// Without the `head` comparison the second case is indistinguishable from the
/// first, and "preserve staged modes" would quietly become "executable bits can
/// never be removed".
///
/// Otherwise one-directional: a path that was `100644` before and `100755` after
/// is a mode the restage discovered on disk, which is the truth to defer to.
/// A path absent from `after` is not restored either — it left the index (safety
/// filter, targeted reset), and re-adding a mode would resurrect it.
pub fn executable_modes_to_restore(
    before: &IndexModes,
    after: &IndexModes,
    head: &IndexModes,
) -> Vec<String> {
    before
        .iter()
        .filter(|(_, was_executable)| **was_executable)
        // Inherited from HEAD, not staged: the disk holds the intent.
        .filter(|(path, _)| head.get(path.as_str()) != Some(&true))
        .filter_map(|(path, _)| match after.get(path) {
            Some(false) => Some(path.clone()),
            // Still executable, or no longer staged at all.
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_executable_and_regular_entries() {
        let output = "100755 6a974bd5d6b70431358d9223e5d53e316bb9cdef 0\tdeploy/gate.sh\n\
                      100644 00eb57578a3810f59994f3c28bb3540824d0177e 0\tsrc/main.rs\n";
        let modes = parse_index_modes(output);
        assert_eq!(modes.get("deploy/gate.sh"), Some(&true));
        assert_eq!(modes.get("src/main.rs"), Some(&false));
    }

    /// A symlink has no executable bit for us to carry. Recording it as
    /// non-executable would be worse than ignoring it: on the next restage it
    /// would look like *lost* intent and this module would propose a
    /// `--chmod=+x` that git refuses on a `120000` entry.
    #[test]
    fn ignores_symlink_and_gitlink_entries() {
        let output = "120000 aaaaaaa 0\tlink-to-thing\n160000 bbbbbbb 0\tvendor/submodule\n";
        assert!(parse_index_modes(output).is_empty());
    }

    /// A conflicted path appears at stages 1/2/3 with no single intent. Picking
    /// one would guess, and a restage right after conflict resolution is the
    /// worst place to guess.
    #[test]
    fn drops_conflicted_paths_entirely() {
        let output = "100755 aaa 1\tconflicted.sh\n\
                      100755 bbb 2\tconflicted.sh\n\
                      100644 ccc 3\tconflicted.sh\n\
                      100755 ddd 0\tclean.sh\n";
        let modes = parse_index_modes(output);
        assert!(
            !modes.contains_key("conflicted.sh"),
            "an unmerged path has no mode intent to preserve"
        );
        assert_eq!(modes.get("clean.sh"), Some(&true));
    }

    /// A stage-0 line for a path that ALSO has conflict stages must not
    /// resurrect it — order within the output must not decide the outcome.
    #[test]
    fn a_stage_zero_line_does_not_rescue_a_conflicted_path() {
        let ordered = "100755 ddd 0\tp.sh\n100755 aaa 2\tp.sh\n";
        let reversed = "100755 aaa 2\tp.sh\n100755 ddd 0\tp.sh\n";
        assert!(parse_index_modes(ordered).is_empty());
        assert!(parse_index_modes(reversed).is_empty());
    }

    #[test]
    fn tolerates_blank_and_malformed_lines() {
        let output = "\nnot-a-real-line\n100755 aaa 0\tok.sh\n100644\n";
        let modes = parse_index_modes(output);
        assert_eq!(modes.len(), 1);
        assert_eq!(modes.get("ok.sh"), Some(&true));
    }

    /// A tree line has one more field than an index line and no stage column.
    /// Handing tree output to `parse_index_modes` reads the object hash as a
    /// stage, marks everything conflicted, and returns nothing — which would
    /// make the HEAD comparison silently vacuous, so both parsers are pinned.
    #[test]
    fn parses_tree_output_which_has_a_type_column() {
        let output = "100755 blob aaaaaaa\tgate.sh\n100644 blob bbbbbbb\tsrc/main.rs\n";
        let modes = parse_tree_modes(output);
        assert_eq!(modes.get("gate.sh"), Some(&true));
        assert_eq!(modes.get("src/main.rs"), Some(&false));

        assert!(
            parse_index_modes(output).is_empty(),
            "the index parser must NOT be reused for tree output"
        );
    }

    #[test]
    fn tree_parser_ignores_symlinks_submodules_and_subtrees() {
        let output = "120000 blob aaa\tlink\n\
                      160000 commit bbb\tvendor/sub\n\
                      040000 tree ccc\tsrcdir\n";
        assert!(parse_tree_modes(output).is_empty());
    }

    /// The tv9g case, reduced: staged 100755, restaged from a 0644 disk file,
    /// and NOT inherited from HEAD.
    #[test]
    fn restores_an_executable_bit_the_restage_dropped() {
        let before = parse_index_modes("100755 aaa 0\tgate.sh\n");
        let after = parse_index_modes("100644 aaa 0\tgate.sh\n");
        let head = IndexModes::new();
        assert_eq!(
            executable_modes_to_restore(&before, &after, &head),
            vec!["gate.sh".to_string()]
        );
    }

    /// Also staged intent: HEAD tracks the path as non-executable, so the
    /// index's 100755 is a change the agent made.
    #[test]
    fn restores_when_head_tracks_the_path_as_non_executable() {
        let before = parse_index_modes("100755 aaa 0\tgate.sh\n");
        let after = parse_index_modes("100644 aaa 0\tgate.sh\n");
        let head = parse_tree_modes("100644 blob aaa\tgate.sh\n");
        assert_eq!(
            executable_modes_to_restore(&before, &after, &head),
            vec!["gate.sh".to_string()]
        );
    }

    /// The inverse hazard, and the reason `head` is a parameter at all.
    ///
    /// HEAD already tracks the file executable, so the index's 100755 was merely
    /// inherited — the agent's real intent is the `chmod -x` it performed on
    /// disk, which the restage correctly picked up. Restoring here would make an
    /// executable bit impossible to REMOVE, turning "preserve staged modes" into
    /// a one-way ratchet.
    #[test]
    fn does_not_revert_a_deliberate_chmod_minus_x_on_a_tracked_file() {
        let before = parse_index_modes("100755 aaa 0\ttool.sh\n");
        let after = parse_index_modes("100644 aaa 0\ttool.sh\n");
        let head = parse_tree_modes("100755 blob aaa\ttool.sh\n");
        assert!(
            executable_modes_to_restore(&before, &after, &head).is_empty(),
            "an intentional chmod -x must survive the restage"
        );
    }

    #[test]
    fn restores_nothing_when_the_bit_survived() {
        let before = parse_index_modes("100755 aaa 0\tgate.sh\n");
        let after = parse_index_modes("100755 aaa 0\tgate.sh\n");
        let head = IndexModes::new();
        assert!(executable_modes_to_restore(&before, &after, &head).is_empty());
    }

    /// One-directional on purpose: a bit the restage *found on disk* is the
    /// truth we defer to. If this ever cleared it, an honest `chmod +x` in the
    /// worktree could never reach a commit.
    #[test]
    fn never_clears_a_bit_the_restage_discovered() {
        let before = parse_index_modes("100644 aaa 0\tgate.sh\n");
        let after = parse_index_modes("100755 aaa 0\tgate.sh\n");
        let head = IndexModes::new();
        assert!(executable_modes_to_restore(&before, &after, &head).is_empty());
    }

    /// A path dropped from the index was dropped deliberately (safety filter,
    /// targeted reset). Restoring its mode would resurrect it.
    #[test]
    fn does_not_restore_a_path_that_left_the_index() {
        let before = parse_index_modes("100755 aaa 0\tsecret.sh\n");
        let after = IndexModes::new();
        let head = IndexModes::new();
        assert!(executable_modes_to_restore(&before, &after, &head).is_empty());
    }

    #[test]
    fn restores_only_the_dropped_subset_of_many_paths() {
        let before = parse_index_modes(
            "100755 a 0\tkept-exec.sh\n\
             100755 b 0\tdropped.sh\n\
             100644 c 0\tplain.rs\n\
             100755 d 0\tunstaged.sh\n\
             100755 e 0\tinherited.sh\n",
        );
        let after = parse_index_modes(
            "100755 a 0\tkept-exec.sh\n\
             100644 b 0\tdropped.sh\n\
             100644 c 0\tplain.rs\n\
             100644 e 0\tinherited.sh\n",
        );
        let head = parse_tree_modes("100755 blob e\tinherited.sh\n");
        assert_eq!(
            executable_modes_to_restore(&before, &after, &head),
            vec!["dropped.sh".to_string()]
        );
    }
}
