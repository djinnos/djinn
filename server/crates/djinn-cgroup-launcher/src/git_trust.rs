//! The git trust anchor a brokered child is born with, and why it is a **file**
//! the launcher owns rather than an environment variable the caller supplies.
//!
//! # The blocker (goxi, sixth in the chain)
//!
//! A task workspace is created by the worker (uid 1000). A brokered command runs
//! as [`CHILD_UID`](crate::child::CHILD_UID) (1001). git compares the repository
//! directory's **owner uid** against the process uid — group ownership and mode
//! do not enter into it — so every brokered `git` invocation dies before it does
//! any work. Measured in a rendered launcher container on the production node:
//!
//! ```text
//! # setpriv --reuid=1001 --regid=1000 --clear-groups git -C /workspace/wt status
//! fatal: detected dubious ownership in repository at '/workspace/wt'
//! ```
//!
//! `djinn-k8s`'s `job.rs` renders `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_0` /
//! `GIT_CONFIG_VALUE_0` = `safe.directory=*` onto the pod for exactly this
//! reason, but [`is_allowed_environment_key`](crate::is_allowed_environment_key)
//! carries no `GIT_` entry, so `process_broker::child_environment` strips all
//! three on the way to the broker. The child is left with no trust rule at all.
//!
//! # Why the fix is not "allow those three keys through"
//!
//! Two independent reasons, and either one alone is decisive.
//!
//! ## 1. `GIT_CONFIG_KEY_n` is an arbitrary-config injection primitive
//!
//! `GIT_CONFIG_COUNT`/`KEY_n`/`VALUE_n` sets *any* configuration key. Several are
//! direct command execution: `core.sshCommand`, `core.pager`, `core.editor`,
//! `diff.external`, `filter.<x>.clean`, `credential.helper`. Allow-listing the
//! key **names** does not help — `GIT_CONFIG_KEY_0` is an allowed name whose
//! *value* is `core.sshCommand`. The forwarded environment originates in the
//! worker, which relays a `Command` an agent tool built, so the value is not
//! trustworthy input. A key-only allow-list would therefore hand every brokered
//! child arbitrary command execution through the one seam that exists to
//! constrain it.
//!
//! This module closes that: the three env-form keys stay **unforwardable**, and
//! the one key that is accepted, `GIT_CONFIG_SYSTEM`, is accepted only when its
//! value is byte-identical to [`anchor_path`] — a path the launcher chose, whose
//! contents the launcher wrote. See
//! [`is_allowed_environment_entry`](crate::env::is_allowed_environment_entry).
//!
//! ## 2. The env form does not actually work (nurw, PR #2575)
//!
//! Even setting security aside, `GIT_CONFIG_COUNT`/`KEY`/`VALUE` is
//! command-scope configuration, and git **strips it from processes it spawns
//! itself**:
//!
//! ```text
//! trace: run_command: unset GIT_CONFIG_COUNT GIT_DIR; git-upload-pack '<src>'
//! ```
//!
//! So `git status` in the worktree is trusted while the `git-upload-pack` child
//! that `git clone --local` uses for ref discovery is not. That is not a corner
//! case here: cloning the `/mirror` PVC is what the workspace is made of. The
//! same measurement was made in `djinn-git/src/lib.rs`, on the same git version
//! that ships in the deployed image, and reproduced locally on git 2.54.0:
//!
//! ```text
//! GIT_TEST_ASSUME_DIFFERENT_OWNER=1 git status
//!   (nothing)                                       -> dubious ownership, FAILS
//!   GIT_CONFIG_SYSTEM=<file holding safe.directory> -> exit 0
//!   GIT_CONFIG_SYSTEM=<file> GIT_CONFIG_NOSYSTEM=1  -> dubious ownership, FAILS
//!   GIT_CONFIG_SYSTEM=<missing file>                -> dubious ownership, FAILS
//! ```
//!
//! `safe.directory` is only honoured from **protected** scope (system/global) in
//! the first place, which is why the answer is a config file and not `git -c`.
//!
//! Re-measured for THIS blocker in a rendered launcher container on the
//! production node, against a real `/workspace/wt` owned `1000:1000` with a real
//! uid-1001 child — no test hook — and it settles the choice on its own:
//!
//! ```text
//! # setpriv --reuid=1001 --regid=1000 --clear-groups ...
//!   (broker strips every GIT_ var — today)   git status        -> rc 128, dubious ownership
//!   GIT_CONFIG_COUNT=1 KEY_0=safe.directory  git status        -> rc 0
//!   GIT_CONFIG_COUNT=1 KEY_0=safe.directory  git clone --local -> rc 128, dubious ownership
//!   GIT_CONFIG_SYSTEM=<anchor>               git status        -> rc 0
//!   GIT_CONFIG_SYSTEM=<anchor>               git clone --local -> rc 0
//! ```
//!
//! So widening the allow-list to those three keys — the "obvious" fix — would
//! have taken the security hit AND still left `git clone --local` broken, which
//! is how the workspace is made.
//!
//! # Why SYSTEM scope and not GLOBAL
//!
//! `djinn_agent_worker`'s `configure_private_dep_access` stores the GitHub
//! installation token as a `url.<...>.insteadOf` rewrite with
//! `git config --global`, and cargo / go / pnpm read it back out of
//! `$HOME/.gitconfig` with djinn nowhere in the loop. Pointing
//! `GIT_CONFIG_GLOBAL` at a launcher-owned file would redirect that read into a
//! file those tools never wrote and silently break every private-dependency
//! fetch. System scope is purely additive: `$HOME/.gitconfig` and the XDG config
//! keep being read exactly as before.
//!
//! # Why the generated file `[include]`s the real system config
//!
//! `GIT_CONFIG_SYSTEM` **replaces** `/etc/gitconfig`; it does not add to it. A
//! bare `[safe] directory = *` file would silently drop whatever the image put
//! in `/etc/gitconfig`. The generated file therefore chains to it, and the
//! include is verified to work in [`tests`] rather than assumed.
//!
//! # The three properties the file itself must have
//!
//! * **Readable by the child.** It is exported to uid 1001; the directory is
//!   `0755` and the file `0644`, set with an explicit `chmod` rather than
//!   inherited from an ambient umask.
//! * **Not writable by the child.** The directory is owned by the launcher uid
//!   and is not group- or other-writable, so uid 1001 cannot replace the file
//!   with one naming `core.sshCommand` — measured on the production node:
//!   `touch` and `>>` both return `Permission denied`. That is re-verified on
//!   every spawn (`lstat`, never following a symlink) and fails closed. The
//!   scratch root ABOVE it needs one repair; see
//!   [`restore_sticky_scratch_semantics`].
//! * **Present.** git ignores a `GIT_CONFIG_SYSTEM` that points at a missing
//!   file *silently* (measured above), so a temp-directory reaper would bring
//!   the dubious-ownership failure back days into a pod's life with nothing in
//!   the logs. Every call re-materializes a file that has gone missing.

use std::io;
use std::path::{Path, PathBuf};

use crate::Error;

/// The one environment variable the launcher injects for git.
pub const GIT_TRUST_ANCHOR_KEY: &str = "GIT_CONFIG_SYSTEM";

/// Basename of the generated config inside [`anchor_dir_in`].
const ANCHOR_BASENAME: &str = "gitconfig";

/// Real system config the generated file chains to when it exists.
const SYSTEM_CONFIG: &str = "/etc/gitconfig";

/// The `safe.directory` value the launcher writes.
///
/// `*` rather than an enumerated list. The check only fires for repositories
/// owned by *another* uid, and the brokered child owns nothing: every path it
/// can reach is a pod volume created by the worker or by the kubelet. Listing
/// them explicitly would mean threading deployment-configured mount roots into
/// the launcher and failing closed — a wedged workspace — the first time a new
/// shared location appeared, which is the exact silent failure this exists to
/// remove.
const SAFE_DIRECTORY_VALUE: &str = "*";

/// Permission bits that must NOT be set on the anchor directory: if the child's
/// group or "other" could write it, the child could replace the anchor.
const FORBIDDEN_DIR_WRITE: u32 = 0o022;

/// Effective uid of this process.
fn effective_uid() -> u32 {
    // SAFETY: `geteuid` always succeeds and takes no arguments.
    unsafe { libc::geteuid() }
}

/// Sticky bit — without it, write permission on a directory is permission to
/// rename or unlink *anything* in it, including entries owned by someone else.
const MODE_STICKY: u32 = 0o1000;
/// Other-write.
const MODE_OTHER_WRITE: u32 = 0o002;

/// Give `root` the `1777` semantics a real `/tmp` has, when we own it.
///
/// An `emptyDir` is not `/tmp`. Measured in a rendered launcher container on the
/// production node, the scratch volume mounted at `/tmp` arrives `2777` —
/// world-writable with **no sticky bit** — so a child holding no privilege at
/// all could displace a root-owned directory inside it:
///
/// ```text
/// # setpriv --reuid=1001 --regid=1000 --clear-groups mv /tmp/djinn-launcher-git-0 /tmp/stolen
/// (rc 0)
/// # chmod 1777 /tmp   ->  3777
/// # setpriv ... mv /tmp/djinn-launcher-git-0 /tmp/stolen2
/// mv: cannot move ...: Operation not permitted   (rc 1)
/// ```
///
/// The ownership check below already fails closed on a displaced directory, so
/// this is not what keeps a hostile config out — it keeps a child from denying
/// git to every *later* command in the pod, and it restores the mode every other
/// process sharing that scratch surface already assumes.
///
/// Best-effort by design: not owning the root is not an error, it just means the
/// fail-closed check is the only protection, which is the pre-existing state.
fn restore_sticky_scratch_semantics(root: &Path) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let Ok(metadata) = std::fs::symlink_metadata(root) else {
        return;
    };
    let mode = metadata.mode();
    if !metadata.is_dir()
        || metadata.uid() != effective_uid()
        || mode & MODE_OTHER_WRITE == 0
        || mode & MODE_STICKY != 0
    {
        return;
    }
    let _ = std::fs::set_permissions(
        root,
        std::fs::Permissions::from_mode((mode & 0o7777) | MODE_STICKY),
    );
}

/// `{root}/djinn-launcher-git-{euid}` — created private to this uid.
///
/// Per-uid so two identities sharing a `/tmp` cannot collide, and an existing
/// directory is accepted only when it is a real directory (never a symlink)
/// owned by us that no other user can write. A pre-planted path therefore
/// cannot be used to feed configuration to a brokered child.
fn anchor_dir_in(root: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};

    restore_sticky_scratch_semantics(root);
    let dir = root.join(format!("djinn-launcher-git-{}", effective_uid()));
    match std::fs::DirBuilder::new().mode(0o755).create(&dir) {
        Ok(()) => {
            // `DirBuilder::mode` is masked by the process umask, and the child
            // must be able to *read* this directory. Set the bits explicitly.
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))?;
            return Ok(dir);
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(err),
    }
    // `symlink_metadata`: never follow a symlink another identity may have
    // planted where we expect a directory.
    let metadata = std::fs::symlink_metadata(&dir)?;
    if !metadata.is_dir()
        || metadata.uid() != effective_uid()
        || metadata.mode() & FORBIDDEN_DIR_WRITE != 0
    {
        return Err(io::Error::other(format!(
            "{} is not a directory owned by uid {} and writable only by it; refusing to \
             read git configuration from it",
            dir.display(),
            effective_uid()
        )));
    }
    Ok(dir)
}

/// Body of the generated system-scope config, chaining to `chain` when present.
pub fn anchor_contents(chain: Option<&Path>) -> String {
    let mut contents = String::from(
        "# Generated by djinn-cgroup-launcher. Do not edit; rewritten on every spawn.\n\
         #\n\
         # A brokered child runs as a uid that owns none of the pod's volumes, so git\n\
         # refuses every repository under them (\"dubious ownership\") without a\n\
         # safe.directory rule in PROTECTED scope. See djinn-cgroup-launcher/src/git_trust.rs.\n",
    );
    if let Some(chain) = chain {
        contents.push_str("[include]\n\tpath = ");
        contents.push_str(&quote_config_value(chain));
        contents.push('\n');
    }
    contents.push_str("[safe]\n\tdirectory = ");
    contents.push_str(SAFE_DIRECTORY_VALUE);
    contents.push('\n');
    contents
}

/// Quote a path as a git config value (git unescapes `\\` and `\"` inside `"`).
fn quote_config_value(path: &Path) -> String {
    let mut quoted = String::from("\"");
    for ch in path.to_string_lossy().chars() {
        if ch == '\\' || ch == '"' {
            quoted.push('\\');
        }
        quoted.push(ch);
    }
    quoted.push('"');
    quoted
}

/// The real system config to chain to, or `None` when there is none to chain.
fn system_config_to_chain(generated: &Path) -> Option<PathBuf> {
    let candidate = PathBuf::from(SYSTEM_CONFIG);
    (candidate != generated && candidate.is_file()).then_some(candidate)
}

/// Write `contents` to `path` atomically, world-readable.
///
/// World-readable because the reader is a *different* uid than the writer by
/// construction. The file holds a `safe.directory` rule and an include path and
/// never a secret. The staging file is created with `O_EXCL` and `O_NOFOLLOW`,
/// so even inside a directory whose ownership check somehow passed, the
/// privileged writer cannot be redirected through a planted symlink.
fn write_anchor(path: &Path, contents: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let staging = path.with_file_name(format!("{ANCHOR_BASENAME}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&staging);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o644)
        .open(&staging)?;
    file.write_all(contents.as_bytes())?;
    drop(file);
    // Explicit, because `OpenOptions::mode` is masked by the process umask and
    // the child must be able to read this file.
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&staging, path)
}

/// Is `path` a regular file this process owns that nobody else can write?
fn anchor_is_intact(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_file()
            && metadata.uid() == effective_uid()
            && metadata.mode() & FORBIDDEN_DIR_WRITE == 0
    })
}

/// (Re-)establish the anchor under `root` and return its path.
///
/// Exposed for tests, which must not share a global anchor. Production goes
/// through [`anchor_path`].
pub fn materialize_in(root: &Path) -> io::Result<PathBuf> {
    let path = anchor_dir_in(root)?.join(ANCHOR_BASENAME);
    let chain = system_config_to_chain(&path);
    materialize_in_with_system(root, chain.as_deref())
}

/// [`materialize_in`], but chaining to `system_config` instead of discovering
/// `/etc/gitconfig`.
///
/// Exists so a behavioural test can be hermetic against the HOST's system
/// config. That is not hypothetical: a GitHub Actions runner ships
///
/// ```text
/// /etc/gitconfig:  [safe]  directory = *
/// ```
///
/// which trusts every repository for every process on the machine. A suite that
/// inherited it would see its non-vacuity control — "with the fix reverted, real
/// git must fail" — pass with exit 0 and empty stderr, and its positive control
/// succeed for a reason that has nothing to do with the launcher. It did exactly
/// that on the first CI run of this change. Pointing the chain at a controlled,
/// empty file models the launcher container, which was measured on the
/// production node to have **no** `/etc/gitconfig` at all.
///
/// The bytes are still produced by [`anchor_contents`] and still written by
/// [`write_anchor`], so what a test exercises is the production writer.
pub fn materialize_in_with_system(
    root: &Path,
    system_config: Option<&Path>,
) -> io::Result<PathBuf> {
    let path = anchor_dir_in(root)?.join(ANCHOR_BASENAME);
    write_anchor(&path, &anchor_contents(system_config))?;
    Ok(path)
}

static ANCHOR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// Path of the launcher's git trust anchor, materialized on first use and
/// re-verified on every call.
///
/// Fails closed. There is deliberately no fallback to the command-scope
/// `GIT_CONFIG_*` form: it is the injection primitive this module exists to keep
/// shut, and nurw measured that it does not survive into `git clone --local`'s
/// inner child anyway, so "falling back" would mean shipping a security hole
/// that does not even fix the bug.
pub fn anchor_path() -> Result<&'static Path, Error> {
    let path = ANCHOR
        .get_or_init(|| materialize_in(&std::env::temp_dir()).ok())
        .as_deref()
        .ok_or(Error::GitTrustAnchorUnavailable)?;

    // Self-heal. A temp-directory reaper can delete the file, and git ignores a
    // `GIT_CONFIG_SYSTEM` pointing at a missing file *silently* — which would
    // bring "dubious ownership" back days into a pod's life with nothing in the
    // logs. One `lstat` per spawn is free next to the fork/exec, and it also
    // catches a file whose ownership or mode changed under us.
    if !anchor_is_intact(path) {
        let contents = anchor_contents(system_config_to_chain(path).as_deref());
        write_anchor(path, &contents).map_err(|_| Error::GitTrustAnchorUnavailable)?;
        if !anchor_is_intact(path) {
            return Err(Error::GitTrustAnchorUnavailable);
        }
    }
    Ok(path)
}

/// Is `value` exactly the launcher's own anchor path?
///
/// The whole value constraint behind `GIT_CONFIG_SYSTEM`'s admission to the
/// forwardable set. A caller-supplied path — which could name a file holding
/// `core.sshCommand` — is not equal to the anchor and is refused.
pub fn is_anchor_value(value: &str) -> bool {
    anchor_path().is_ok_and(|anchor| anchor == Path::new(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "djinn-git-trust-test-{}-{name}-{}",
            std::process::id(),
            effective_uid()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create test root");
        root
    }

    #[test]
    fn the_anchor_is_readable_by_another_uid_and_writable_by_none() {
        let root = temp_root("modes");
        let path = materialize_in(&root).expect("materialize");
        let file = std::fs::symlink_metadata(&path).expect("stat anchor");
        let dir = std::fs::symlink_metadata(path.parent().expect("anchor has a parent"))
            .expect("stat anchor dir");
        // The reader is uid 1001 and the writer is the launcher uid: without
        // world-read the child cannot load the trust rule at all.
        assert_eq!(file.mode() & 0o777, 0o644, "anchor must be world-readable");
        assert_eq!(dir.mode() & 0o777, 0o755, "anchor dir must be traversable");
        assert_eq!(
            file.mode() & FORBIDDEN_DIR_WRITE,
            0,
            "the child must not be able to rewrite the anchor"
        );
        assert_eq!(file.uid(), effective_uid());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `OpenOptions::mode` and `DirBuilder::mode` are both masked by the process
    /// umask. This repo has already shipped a umask-022 regression that broke
    /// the warm path, so the anchor's readability may not depend on one.
    #[test]
    fn a_hostile_umask_cannot_make_the_anchor_unreadable() {
        let root = temp_root("umask");
        // SAFETY: `umask` cannot fail; restored before the test returns.
        let previous = unsafe { libc::umask(0o077) };
        let path = materialize_in(&root);
        unsafe { libc::umask(previous) };
        let path = path.expect("materialize under umask 077");
        assert_eq!(
            std::fs::symlink_metadata(&path)
                .expect("stat anchor")
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(
            std::fs::symlink_metadata(path.parent().expect("parent"))
                .expect("stat dir")
                .mode()
                & 0o777,
            0o755
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_anchor_grants_safe_directory_and_nothing_else() {
        let contents = anchor_contents(None);
        assert!(contents.contains("[safe]"));
        assert!(contents.contains("directory = *"));
        for dangerous in [
            "sshCommand",
            "pager",
            "editor",
            "credential",
            "insteadOf",
            "external",
        ] {
            assert!(
                !contents.contains(dangerous),
                "the anchor must never carry {dangerous}"
            );
        }
    }

    /// `GIT_CONFIG_SYSTEM` REPLACES `/etc/gitconfig`; a bare anchor would drop
    /// whatever the image put there.
    #[test]
    fn the_anchor_chains_to_the_real_system_config_when_one_exists() {
        let chained = anchor_contents(Some(Path::new("/etc/gitconfig")));
        assert!(chained.contains("[include]"));
        assert!(chained.contains("path = \"/etc/gitconfig\""));
        assert!(
            !anchor_contents(None).contains("[include]"),
            "no include when there is nothing to chain to"
        );
    }

    #[test]
    fn a_directory_another_identity_could_write_is_refused() {
        let root = temp_root("hostile-dir");
        let planted = root.join(format!("djinn-launcher-git-{}", effective_uid()));
        std::fs::create_dir(&planted).expect("plant directory");
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o777))
            .expect("make it group/other-writable");
        let error = materialize_in(&root).expect_err("a writable-by-others dir must be refused");
        assert!(
            error.to_string().contains("writable only by it"),
            "unexpected error: {error}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlink_where_the_directory_belongs_is_refused_without_being_followed() {
        let root = temp_root("symlink");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir(&elsewhere).expect("create target");
        std::os::unix::fs::symlink(
            &elsewhere,
            root.join(format!("djinn-launcher-git-{}", effective_uid())),
        )
        .expect("plant symlink");
        assert!(materialize_in(&root).is_err());
        assert!(
            !elsewhere.join(ANCHOR_BASENAME).exists(),
            "the symlink must not have been followed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// git ignores a `GIT_CONFIG_SYSTEM` pointing at a missing file silently, so
    /// a reaper deleting it must not go unnoticed.
    /// An `emptyDir` mounted at `/tmp` arrives `2777` — world-writable with no
    /// sticky bit — so write permission on it is permission to rename a
    /// root-owned directory out of the way. Measured on the production node.
    #[test]
    fn a_world_writable_scratch_root_regains_its_sticky_bit() {
        let root = temp_root("sticky");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o2777))
            .expect("model the rendered emptyDir mode");
        materialize_in(&root).expect("materialize");
        let mode = std::fs::symlink_metadata(&root).expect("stat").mode();
        assert_eq!(
            mode & MODE_STICKY,
            MODE_STICKY,
            "an other-writable scratch root we own must regain sticky semantics, or a \
             child can displace the anchor directory (measured: `mv` succeeded at 2777, \
             failed at 3777)"
        );
        assert_eq!(
            mode & 0o2777,
            0o2777,
            "and nothing else about the mode may change"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Hardening a root we do NOT own is not ours to do, and failing there would
    /// take out every brokered command.
    #[test]
    fn a_root_that_is_not_ours_or_is_already_sticky_is_left_alone() {
        let already = temp_root("already-sticky");
        std::fs::set_permissions(&already, std::fs::Permissions::from_mode(0o1777)).expect("chmod");
        restore_sticky_scratch_semantics(&already);
        assert_eq!(
            std::fs::symlink_metadata(&already).expect("stat").mode() & 0o7777,
            0o1777
        );
        // A private root is not other-writable and must not gain a sticky bit.
        let private = temp_root("private");
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        restore_sticky_scratch_semantics(&private);
        assert_eq!(
            std::fs::symlink_metadata(&private).expect("stat").mode() & 0o7777,
            0o755
        );
        // A path that is not a directory at all is a no-op, not a panic.
        restore_sticky_scratch_semantics(Path::new("/dev/null"));
        let _ = std::fs::remove_dir_all(&already);
        let _ = std::fs::remove_dir_all(&private);
    }

    #[test]
    fn a_deleted_anchor_is_re_materialized() {
        let root = temp_root("reaper");
        let path = materialize_in(&root).expect("materialize");
        std::fs::remove_file(&path).expect("simulate a temp reaper");
        assert!(!anchor_is_intact(&path));
        let again = materialize_in(&root).expect("re-materialize");
        assert_eq!(again, path);
        assert!(anchor_is_intact(&path));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_production_anchor_is_stable_and_recognized_only_as_itself() {
        let anchor = anchor_path().expect("the anchor must materialize in a test environment");
        assert_eq!(anchor, anchor_path().expect("stable"));
        assert!(is_anchor_value(&anchor.display().to_string()));
        for impostor in [
            "/workspace/.gitconfig",
            "/home/djinn/.gitconfig",
            "/etc/gitconfig",
            "",
        ] {
            assert!(
                !is_anchor_value(impostor),
                "{impostor} must not pass as the launcher's anchor"
            );
        }
    }
}
