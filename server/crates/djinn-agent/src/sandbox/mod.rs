// Sandbox module — OS-level shell sandbox trait and backend selection.
//
// ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt
// ADR-017: Worktree Injection and Landlock Crate

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::Result;

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
    fn apply(&self, worktree_path: &Path, cmd: &mut std::process::Command) -> Result<()>;
}

// ─── Global singleton ─────────────────────────────────────────────────────────

/// Global sandbox backend, detected once at first use.
pub static SANDBOX: LazyLock<Box<dyn Sandbox>> = LazyLock::new(detect_backend);

// ─── FallbackSandbox ─────────────────────────────────────────────────────────

/// Fallback sandbox: heuristic path validation for kernels that do not support
/// Landlock (< 5.13, WSL1) or non-Linux/macOS platforms.
///
/// Validates that `worktree_path` is inside a `.djinn/worktrees/` subtree or a
/// well-known temp directory. Does not apply OS-level access controls.
pub struct FallbackSandbox;

impl Sandbox for FallbackSandbox {
    fn apply(&self, worktree_path: &Path, _cmd: &mut std::process::Command) -> Result<()> {
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
pub(crate) fn djinn_cache_dir() -> Option<PathBuf> {
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
        .any(|w| w[0] == ".djinn" && w[1] == "worktrees")
}

// ─── Git worktree metadata resolution ─────────────────────────────────────────

/// Resolve the git worktree metadata directory for a `.djinn/worktrees/{id}/` path.
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
fn probe_landlock() -> bool {
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
    ret > 0
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
