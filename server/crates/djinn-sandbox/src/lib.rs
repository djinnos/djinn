// Sandbox module — OS-level shell sandbox trait and backend selection.
//
// ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt
// ADR-017: Worktree Injection and Landlock Crate

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::Result;

/// Strict reusable-final-verification launcher. Unlike [`SANDBOX`], this
/// module never selects the heuristic fallback backend.
#[cfg(target_os = "linux")]
pub mod final_verification;
#[cfg(target_os = "linux")]
pub mod final_verification_execution;
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod chat_shell;
// Re-exports used by the chat handler in commits 5 and 6; suppress the
// unused-import warning until the rewire lands.
#[cfg(target_os = "linux")]
#[allow(unused_imports)]
pub use chat_shell::{ChatShellError, ChatShellRequest, ChatShellResult, ChatShellSandbox};

// ─── Trait ────────────────────────────────────────────────────────────────────

/// Policy enforcement interface for agent shell calls.
///
/// `apply` is called just before the child process is spawned. Implementations
/// restrict what the process can read and write using OS-level primitives
/// (Landlock on Linux, Seatbelt on macOS). FallbackSandbox performs heuristic
/// path validation when the OS backend is unavailable.
pub trait Sandbox: Send + Sync {
    fn apply(&self, scope: SandboxScope<'_>, cmd: &mut std::process::Command) -> Result<()>;
}

/// Typed filesystem authority for one shell invocation. Keeping read sources
/// distinct prevents backends from ever adding them to a writable allowlist.
#[derive(Clone, Copy, Debug)]
pub enum SandboxScope<'a> {
    Worktree(&'a Path),
    ReadSource { root: &'a Path, cwd: &'a Path },
}

impl SandboxScope<'_> {
    pub fn validate(self) -> Result<()> {
        match self {
            Self::Worktree(path) => validate_directory(path, "worktree"),
            Self::ReadSource { root, cwd } => {
                validate_directory(root, "read-source root")?;
                validate_directory(cwd, "read-source cwd")?;
                reject_read_source_writable_overlap(root)?;
                anyhow::ensure!(
                    cwd.canonicalize()?.starts_with(root.canonicalize()?),
                    "read-source cwd escapes mounted root"
                );
                Ok(())
            }
        }
    }
}

fn validate_directory(path: &Path, label: &str) -> Result<()> {
    anyhow::ensure!(
        path.is_dir(),
        "{label} does not exist or is not a directory: {}",
        path.display()
    );
    Ok(())
}

/// Read-source roots must not be nested under a backend-wide writable rule.
///
/// Both OS backends intentionally allow these locations for compiler caches and
/// disk-backed scratch. Landlock and Seatbelt use additive path rules, so a
/// read-only child cannot override a writable ancestor. Reject the scope before
/// spawning: an authorized cache mounted there must be remounted elsewhere.
fn reject_read_source_writable_overlap(root: &Path) -> Result<()> {
    let root = root.canonicalize()?;
    let mut writable_roots = vec![PathBuf::from("/cache"), PathBuf::from("/var/tmp")];
    if let Some(cache) = djinn_cache_dir() {
        writable_roots.push(cache);
    }
    if let Some(cargo_home) = std::env::var_os("CARGO_HOME") {
        writable_roots.push(PathBuf::from(cargo_home).join("build"));
    } else if let Some(home) = std::env::var_os("HOME") {
        writable_roots.push(PathBuf::from(home).join(".cargo/build"));
    }

    for writable_root in writable_roots {
        // A missing optional cache directory cannot contain the validated,
        // existing read-source root, so retain its lexical path in that case.
        let writable_root = writable_root.canonicalize().unwrap_or(writable_root);
        anyhow::ensure!(
            !root.starts_with(&writable_root),
            "read-source root overlaps a writable sandbox path: {}",
            writable_root.display()
        );
    }
    Ok(())
}

// ─── Global singleton ─────────────────────────────────────────────────────────

/// Global sandbox backend, detected once at first use.
pub static SANDBOX: LazyLock<Box<dyn Sandbox>> = LazyLock::new(detect_backend);

// ─── FallbackSandbox ─────────────────────────────────────────────────────────

/// Fallback sandbox: heuristic path validation for kernels that do not support
/// Landlock (< 5.13, WSL1) or non-Linux/macOS platforms.
///
/// Validates that `worktree_path` is inside a Release N task-worktree subtree
/// (`.task-runtime/worktrees/`, or `.djinn/worktrees/` for an active legacy
/// worktree) or a well-known temp directory. Does not apply OS-level access
/// controls.
pub struct FallbackSandbox;

impl Sandbox for FallbackSandbox {
    fn apply(&self, scope: SandboxScope<'_>, _cmd: &mut std::process::Command) -> Result<()> {
        if matches!(scope, SandboxScope::ReadSource { .. }) {
            return Err(anyhow::anyhow!(
                "read-source shell requires an OS read-only sandbox"
            ));
        }
        let SandboxScope::Worktree(worktree_path) = scope else {
            unreachable!()
        };
        if !worktree_path.exists() || !worktree_path.is_dir() {
            return Err(anyhow::anyhow!(
                "workdir does not exist or is not a directory: {}",
                worktree_path.display()
            ));
        }
        if is_worktree_path(worktree_path) || is_temp_path(worktree_path) {
            return Ok(());
        }
        Err(anyhow::anyhow!(
            "workdir is outside task worktree: {}",
            worktree_path.display()
        ))
    }
}

fn is_temp_path(path: &Path) -> bool {
    if path.starts_with("/var/tmp") {
        return true;
    }
    // Accept the djinn agent scratch dir under the user's cache directory.
    // Resolve env vars at check time since this is a pure path validator
    // and we have no filesystem state to rely on.
    if let Some(cache) = djinn_cache_dir()
        && path.starts_with(&cache)
    {
        return true;
    }
    false
}

/// Resolve the djinn agent scratch cache directory.
///
/// Returns `$XDG_CACHE_HOME/djinn` if `XDG_CACHE_HOME` is set, else
/// `$HOME/.cache/djinn` if `HOME` is set, else `None`. This is the standard
/// place for sandboxed agents to write scratch state (replaces ad-hoc use of
/// `/tmp`). Both the Linux Landlock backend and the macOS Seatbelt backend
/// allow writes beneath this path; the fallback heuristic accepts it too.
pub fn djinn_cache_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg).join("djinn"));
    }
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".cache").join("djinn"))
}

fn is_worktree_path(path: &Path) -> bool {
    let parts: Vec<String> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    parts
        .windows(2)
        .any(|w| (w[0] == ".task-runtime" || w[0] == ".djinn") && w[1] == "worktrees")
}

// ─── Git worktree metadata resolution ─────────────────────────────────────────

/// Resolve the git worktree metadata directory for a `.task-runtime/worktrees/{id}/` path.
///
/// Git stores per-worktree state (HEAD, ORIG_HEAD, index, refs) under
/// `<repo>/.git/worktrees/{id}/`. Operations like `git merge` need write
/// access there (e.g. ORIG_HEAD.lock). Returns `None` if the `.git` file
/// doesn't point to a recognizable worktree metadata path.
pub fn git_metadata_dir(worktree: &Path) -> Option<PathBuf> {
    let dot_git = worktree.join(".git");
    let content = std::fs::read_to_string(&dot_git).ok()?;
    // .git file contains: "gitdir: ../../.git/worktrees/{id}"
    let gitdir = content.strip_prefix("gitdir: ")?.trim();
    let resolved = if Path::new(gitdir).is_absolute() {
        PathBuf::from(gitdir)
    } else {
        worktree.join(gitdir).canonicalize().ok()?
    };
    if resolved.is_dir() {
        Some(resolved)
    } else {
        None
    }
}

/// Resolve the main `.git/` directory for a worktree.
///
/// The worktree's `.git` file points to `<repo>/.git/worktrees/{id}`, so the
/// main `.git/` dir is two levels up. Write access to this directory is needed
/// for merge operations that write objects, refs, and packed-refs.
pub fn git_dir(worktree: &Path) -> Option<PathBuf> {
    let meta = git_metadata_dir(worktree)?;
    // meta = <repo>/.git/worktrees/{id} — go up two levels to reach .git/
    meta.parent()?.parent().map(PathBuf::from)
}

// ─── Backend detection ────────────────────────────────────────────────────────

/// Probe the OS and return the best available sandbox backend.
///
/// On Linux, attempts to create a Landlock ruleset to verify kernel support
/// (≥ 5.13). If unavailable, returns `FallbackSandbox` with a warning.
///
/// On macOS, returns the Seatbelt-based sandbox.
///
/// On all other platforms, returns `FallbackSandbox`.
///
/// This function should be called once at startup and the result stored in
/// supervisor state.
pub fn detect_backend() -> Box<dyn Sandbox> {
    _detect()
}

#[cfg(target_os = "linux")]
fn _detect() -> Box<dyn Sandbox> {
    if probe_landlock() {
        tracing::info!("sandbox: Landlock available, using LandlockSandbox");
        return Box::new(linux::LandlockSandbox);
    }
    tracing::warn!(
        "sandbox: Landlock unavailable (kernel < 5.13 or WSL1), \
         falling back to FallbackSandbox heuristics"
    );
    Box::new(FallbackSandbox)
}

#[cfg(target_os = "macos")]
fn _detect() -> Box<dyn Sandbox> {
    tracing::info!("sandbox: macOS detected, using SeatbeltSandbox");
    Box::new(macos::SeatbeltSandbox)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn _detect() -> Box<dyn Sandbox> {
    tracing::warn!("sandbox: unsupported platform, falling back to FallbackSandbox heuristics");
    Box::new(FallbackSandbox)
}

// ─── Linux Landlock probe ─────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
pub(crate) fn probe_landlock() -> bool {
    landlock_abi().is_some()
}

/// Return the Landlock ABI reported by the running kernel.
///
/// Callers with stricter policies must compare this value with the ABI whose
/// access set they request; basic agent-shell backend detection intentionally
/// continues to accept every supported ABI.
#[cfg(target_os = "linux")]
pub(crate) fn landlock_abi() -> Option<i32> {
    // landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION=1)
    // Returns the Landlock ABI version (> 0) if the kernel supports it,
    // or -ENOSYS if Landlock is not available. Syscall 444 is stable on
    // x86_64, arm64, and riscv64.
    let ret = unsafe {
        libc::syscall(
            444,                              // SYS_landlock_create_ruleset
            std::ptr::null::<libc::c_void>(), // attr = NULL
            0usize,                           // size = 0
            1i32,                             // flags = LANDLOCK_CREATE_RULESET_VERSION
        )
    };
    (ret > 0).then_some(ret as i32)
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize env-dependent tests: each test fully restores env before returning.
    /// Because Rust test threads share a process, env mutations race across tests.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| ((*k).to_string(), std::env::var(*k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        for (k, v) in saved {
            match v {
                Some(val) => unsafe { std::env::set_var(&k, val) },
                None => unsafe { std::env::remove_var(&k) },
            }
        }
        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    #[test]
    fn djinn_cache_dir_prefers_xdg_cache_home() {
        with_env(
            &[
                ("XDG_CACHE_HOME", Some("/xdg-cache")),
                ("HOME", Some("/home/alice")),
            ],
            || {
                assert_eq!(djinn_cache_dir(), Some(PathBuf::from("/xdg-cache/djinn")));
            },
        );
    }

    #[test]
    fn djinn_cache_dir_falls_back_to_home_dot_cache() {
        with_env(
            &[("XDG_CACHE_HOME", None), ("HOME", Some("/home/bob"))],
            || {
                assert_eq!(
                    djinn_cache_dir(),
                    Some(PathBuf::from("/home/bob/.cache/djinn"))
                );
            },
        );
    }

    #[test]
    fn djinn_cache_dir_none_when_neither_env_set() {
        with_env(&[("XDG_CACHE_HOME", None), ("HOME", None)], || {
            assert_eq!(djinn_cache_dir(), None);
        });
    }

    #[test]
    fn worktree_path_accepts_destination_and_active_legacy_paths() {
        assert!(is_worktree_path(Path::new(
            "/projects/acme/repo/.task-runtime/worktrees/task-1"
        )));
        assert!(is_worktree_path(Path::new(
            "/projects/acme/repo/.djinn/worktrees/task-1"
        )));
        assert!(!is_worktree_path(Path::new(
            "/projects/acme/repo/.task-runtime/read-sources/other"
        )));
    }

    #[test]
    fn read_source_scope_rejects_cwd_outside_owner_cache() {
        // The process temp directory is intentionally writable in both OS
        // policies. Put this fixture under the test working directory so the
        // overlap guard cannot mask the CWD-containment error this regression
        // is meant to exercise.
        let fixture_parent = std::env::current_dir().expect("test working directory");
        let root = tempfile::tempdir_in(&fixture_parent).expect("root");
        let outside = tempfile::tempdir_in(&fixture_parent).expect("outside");
        let error = SandboxScope::ReadSource {
            root: root.path(),
            cwd: outside.path(),
        }
        .validate()
        .expect_err("read-source cwd must remain in its root");
        assert!(error.to_string().contains("escapes mounted root"));
    }

    #[test]
    fn read_source_scope_rejects_writable_scratch_overlap() {
        let root = tempfile::tempdir_in("/var/tmp").expect("root");
        let error = SandboxScope::ReadSource {
            root: root.path(),
            cwd: root.path(),
        }
        .validate()
        .expect_err("read-source roots cannot inherit /var/tmp write access");
        assert!(
            error
                .to_string()
                .contains("overlaps a writable sandbox path: /var/tmp")
        );
    }

    #[test]
    fn fallback_rejects_read_source_scope() {
        let root = tempfile::tempdir().expect("root");
        let mut command = std::process::Command::new("true");
        let error = FallbackSandbox
            .apply(
                SandboxScope::ReadSource {
                    root: root.path(),
                    cwd: root.path(),
                },
                &mut command,
            )
            .expect_err("fallback must reject a read-source scope");
        assert!(
            error
                .to_string()
                .contains("requires an OS read-only sandbox")
        );
    }

    #[test]
    fn is_temp_path_rejects_slash_tmp() {
        with_env(
            &[("XDG_CACHE_HOME", None), ("HOME", Some("/home/carol"))],
            || {
                assert!(!is_temp_path(Path::new("/tmp")));
                assert!(!is_temp_path(Path::new("/tmp/foo")));
            },
        );
    }

    #[test]
    fn is_temp_path_accepts_var_tmp_and_cache_dir() {
        with_env(
            &[("XDG_CACHE_HOME", None), ("HOME", Some("/home/dave"))],
            || {
                assert!(is_temp_path(Path::new("/var/tmp")));
                assert!(is_temp_path(Path::new("/var/tmp/scratch")));
                assert!(is_temp_path(Path::new("/home/dave/.cache/djinn")));
                assert!(is_temp_path(Path::new("/home/dave/.cache/djinn/x")));
                assert!(!is_temp_path(Path::new("/home/dave/.cache/other")));
            },
        );
    }
}
