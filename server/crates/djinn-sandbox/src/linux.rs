// Linux Landlock sandbox backend.
//
// ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt
// ADR-017: Shell Sandbox Implementation — Worktree Injection and Landlock Crate

#![cfg(target_os = "linux")]

use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use landlock::{
    ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr,
};

use crate::{Sandbox, SandboxScope, djinn_cache_dir, git_dir, git_metadata_dir};

/// Landlock-based filesystem sandbox for Linux ≥ 5.13.
///
/// Restricts the agent child process to read-everywhere, write only to the
/// task worktree, its git metadata directory, `/var/tmp`, a dedicated djinn
/// agent scratch dir (`$XDG_CACHE_HOME/djinn` or `$HOME/.cache/djinn`), the
/// shared cross-task cache PVC (`/cache`, where toolchain caches like
/// `GOMODCACHE`/`GOCACHE`/sccache live, task-run Cargo targets use private
/// `/cache/cargo-target-runs/<task_run_id>` dirs, and warm jobs maintain
/// `/cache/cargo-target/<project_id>` bases), and the usual `/dev/{null,zero,urandom}`
/// nodes. `/tmp` is intentionally not
/// writable: on typical Linux it's tmpfs, and allowing writes there caused a
/// 3.8 GB cargo-artifact leak into RAM-backed storage.
pub struct LandlockSandbox;

impl Sandbox for LandlockSandbox {
    fn apply(&self, scope: SandboxScope<'_>, cmd: &mut std::process::Command) -> Result<()> {
        use std::os::unix::process::CommandExt;

        scope.validate()?;

        // Redirect temp to a Landlock-writable, disk-backed dir. The K8s task-run
        // Pod sets TMPDIR=/workspace (job.rs) so the host supervisor's TempDir
        // lands on the big writable `/workspace` emptyDir — but that's the PVC
        // mount ROOT, and the rules below grant the agent write access only to
        // its worktree SUBDIR (`/workspace/<project>`). So any sandboxed tool
        // that honors `$TMPDIR` — go's git codehost (`go-codehost-*`), cargo/cc
        // linker scratch, etc. — was creating temp files directly under
        // `/workspace` and hitting `permission denied`. Point sandboxed commands
        // at `/var/tmp`, which is already in the writable allowlist (the
        // `/var/tmp` rule below requires it to exist). `GOTMPDIR` is unset in the
        // image, so Go falls back to `$TMPDIR` — this covers it without a
        // Go-specific knob. The supervisor's own TempDir is unaffected: it is not
        // spawned through this sandbox, so it keeps using the `/workspace`
        // emptyDir for large mirror clones.
        cmd.env("TMPDIR", "/var/tmp");

        let (writable_worktree, git_meta) = match scope {
            SandboxScope::Worktree(path) => (Some(path.to_path_buf()), git_metadata_dir(path)),
            SandboxScope::ReadSource { .. } => (None, None),
        };

        // Resolve + create the djinn cache dir in the PARENT process, before
        // fork. `create_dir_all` and `tracing::warn!` are not async-signal-safe,
        // so they must not run inside `pre_exec` — doing so risks deadlocking
        // a forked child if another thread in the tokio-based parent held a
        // malloc/tracing mutex at fork time. Only the Landlock ruleset
        // construction runs post-fork in pre_exec.
        let cache_dir_for_rule = prepare_cache_dir();

        // Safety: pre_exec runs in the forked child process. The closure only
        // performs Landlock syscalls and open(2) calls, both of which are
        // async-signal-safe per POSIX.
        unsafe {
            cmd.pre_exec(move || {
                apply_policy(
                    writable_worktree.as_deref(),
                    git_meta.as_deref(),
                    cache_dir_for_rule.as_deref(),
                )
                .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e.to_string()))
            });
        }
        Ok(())
    }
}

/// Resolve the djinn agent scratch directory and ensure it exists.
///
/// Runs in the parent process only. Returns `Some(path)` if the directory
/// exists (either already present or successfully created), `None` otherwise.
/// On creation failure, logs a warning and returns `None` so the sandbox
/// setup can continue without the cache-dir allowance.
fn prepare_cache_dir() -> Option<PathBuf> {
    let dir = djinn_cache_dir()?;
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            tracing::warn!(
                path = %dir.display(),
                error = %e,
                "sandbox: failed to create djinn cache dir; skipping Landlock rule"
            );
            None
        }
    }
}

/// Build and apply the Landlock policy in the current process.
///
/// Called inside `pre_exec` (forked child) so it takes effect before exec.
/// Only async-signal-safe operations are performed here: Landlock syscalls
/// and `open(2)` via `PathFd::new`. Path resolution, directory creation,
/// logging, and any allocator-heavy work must happen in the parent before
/// fork — see `LandlockSandbox::apply` and `prepare_cache_dir`.
fn apply_policy(
    worktree: Option<&Path>,
    git_meta: Option<&Path>,
    cache_dir: Option<&Path>,
) -> anyhow::Result<()> {
    // Use V3 (Linux 5.19+). The probe in mod.rs verified the kernel supports
    // Landlock; V3 covers all practical kernels in 2026.
    let abi = ABI::V3;
    let full_access = AccessFs::from_all(abi);

    // Read-only subset: allow read and execute, deny all write operations.
    let read_exec = AccessFs::Execute | AccessFs::ReadFile | AccessFs::ReadDir;

    // Cargo shared build cache: {CARGO_HOME}/build/ (default ~/.cargo/build/).
    // Agents need write access so `cargo test`/`cargo clippy` can use the shared
    // build-dir configured in .cargo/config.toml.
    let cargo_build_dir = std::env::var("CARGO_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.cargo")))
        .ok()
        .map(|base| std::path::PathBuf::from(base).join("build"));

    let mut ruleset = Ruleset::default()
        .handle_access(full_access)?
        .create()?
        // Read + execute access everywhere on the filesystem.
        .add_rule(PathBeneath::new(PathFd::new("/")?, read_exec))?
        // Full access to /var/tmp (disk-backed) and /dev/null et al.
        // /tmp is intentionally excluded: on Linux it's typically tmpfs and
        // writes there can silently consume RAM.
        .add_rule(PathBeneath::new(PathFd::new("/var/tmp")?, full_access))?
        .add_rule(PathBeneath::new(PathFd::new("/dev/null")?, full_access))?
        .add_rule(PathBeneath::new(PathFd::new("/dev/zero")?, full_access))?
        .add_rule(PathBeneath::new(PathFd::new("/dev/urandom")?, full_access))?;

    if let Some(worktree) = worktree {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(worktree)?, full_access))?;
    }

    // Cargo shared build cache directory.
    if let Some(ref dir) = cargo_build_dir.filter(|d| d.is_dir()) {
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(dir)?, full_access))?;
    }

    // Djinn agent scratch dir. The directory was already resolved and created
    // in the parent (see `prepare_cache_dir`) so here we only need to open it
    // and add the rule. If the open fails for any reason, silently skip: we
    // cannot log safely from pre_exec, and the sandbox is still functional
    // without the scratch allowance.
    if let Some(dir) = cache_dir
        && let Ok(fd) = PathFd::new(dir)
    {
        ruleset = ruleset.add_rule(PathBeneath::new(fd, full_access))?;
    }

    // Shared cross-task cache PVC (`/cache`). The K8s task-run Pod env
    // (djinn-k8s/src/job.rs) routes the toolchain caches here at runtime —
    // CARGO_HOME=/cache/cargo,
    // CARGO_TARGET_DIR=/cache/cargo-target-runs/<task_run_id> for private
    // per-run Cargo target dirs seeded from warm bases when available,
    // SCCACHE_DIR=/cache/sccache/<project> — and the image bakes the Go cache
    // (GOMODCACHE/GOCACHE) onto /cache too. Warm jobs maintain shared base
    // targets under /cache/cargo-target/<project_id>. The broad /cache rule is
    // compatible with both path families and lets build/test commands populate
    // their assigned cache locations (`go mod download` → /cache/go/mod, cargo
    // registry → /cache/cargo, private cargo target artifacts →
    // /cache/cargo-target-runs, warm base maintenance → /cache/cargo-target,
    // sccache → /cache/sccache, etc.). Only present in the K8s task-run Pod
    // (the PVC mount); a no-op elsewhere since the open fails. Guarded:
    // if the dir is absent we silently skip, same as the scratch dir above.
    if let Ok(fd) = PathFd::new("/cache") {
        ruleset = ruleset.add_rule(PathBeneath::new(fd, full_access))?;
    }

    // Full .git/ dir needs write access for merge operations: object writes
    // (.git/objects/), ref updates (.git/refs/, .git/packed-refs), and
    // per-worktree state (.git/worktrees/{id}/ORIG_HEAD.lock etc.).
    if let Some(dot_git) = worktree.and_then(git_dir) {
        if dot_git.is_dir() {
            ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(&dot_git)?, full_access))?;
        }
    } else if let Some(meta) = git_meta {
        // Fallback: at least allow the worktree metadata dir.
        ruleset = ruleset.add_rule(PathBeneath::new(PathFd::new(meta)?, full_access))?;
    }

    ruleset.restrict_self()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};

    fn write_file(scope: SandboxScope<'_>, path: &Path) -> std::process::Output {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "printf x > \"$1\"", "--"]).arg(path);
        LandlockSandbox
            .apply(scope, &mut cmd)
            .expect("scope should configure Landlock");
        cmd.output().expect("sandboxed shell should spawn")
    }

    /// The task-run Pod inherits `TMPDIR=/workspace` (the read-only PVC mount
    /// root). The sandbox must override it to a Landlock-writable dir, or every
    /// sandboxed tool that honors `$TMPDIR` (go codehost, cargo/cc linker) hits
    /// `permission denied` writing temp under `/workspace`.
    #[test]
    fn apply_redirects_tmpdir_to_var_tmp() {
        let mut cmd = std::process::Command::new("true");
        cmd.env("TMPDIR", "/workspace");

        LandlockSandbox
            .apply(SandboxScope::Worktree(Path::new("/tmp")), &mut cmd)
            .expect("apply should succeed");

        let tmpdir = cmd
            .get_envs()
            .find(|(k, _)| *k == OsStr::new("TMPDIR"))
            .and_then(|(_, v)| v)
            .map(|v| v.to_owned());
        assert_eq!(
            tmpdir,
            Some(OsString::from("/var/tmp")),
            "sandboxed commands must use a Landlock-writable TMPDIR, not the inherited /workspace"
        );
    }

    /// Exercise the actual Landlock policy: an owner-cache source is readable
    /// but neither its files nor its Git metadata can be changed, while a task
    /// worktree remains writable.
    #[test]
    fn read_source_policy_denies_content_and_git_writes_but_allows_worktree() {
        if !crate::probe_landlock() {
            return;
        }
        let source = tempfile::tempdir_in(std::env::current_dir().expect("test directory"))
            .expect("read source");
        let source_git = source.path().join(".git");
        std::fs::create_dir(&source_git).expect("source git directory");
        let worktree = tempfile::tempdir_in("/var/tmp").expect("worktree");

        let source_content = source.path().join("source-write");
        assert!(
            !write_file(
                SandboxScope::ReadSource {
                    root: source.path(),
                    cwd: source.path(),
                },
                &source_content,
            )
            .status
            .success(),
            "Landlock must deny writes to read-source content"
        );
        assert!(!source_content.exists());

        let source_metadata = source_git.join("metadata-write");
        assert!(
            !write_file(
                SandboxScope::ReadSource {
                    root: source.path(),
                    cwd: source.path(),
                },
                &source_metadata,
            )
            .status
            .success(),
            "Landlock must deny writes to read-source Git metadata"
        );
        assert!(!source_metadata.exists());

        let worktree_content = worktree.path().join("worktree-write");
        assert!(
            write_file(SandboxScope::Worktree(worktree.path()), &worktree_content)
                .status
                .success(),
            "Landlock must retain task-worktree write access"
        );
        assert!(worktree_content.exists());
    }
}
