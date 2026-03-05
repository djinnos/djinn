// Sandbox module — OS-level shell sandbox trait and backend selection.
//
// ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt
// ADR-017: Worktree Injection and Landlock Crate

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::Result;

pub mod linux;
pub mod macos;

// ─── Trait ────────────────────────────────────────────────────────────────────

/// Policy enforcement interface for agent shell calls.
///
/// `apply` is called just before the child process is spawned. Implementations
/// restrict what the process can read and write using OS-level primitives
/// (Landlock on Linux, Seatbelt on macOS). FallbackSandbox performs heuristic
/// path validation when the OS backend is unavailable.
pub trait Sandbox: Send + Sync {
    fn apply(&self, worktree_path: &Path, cmd: &mut tokio::process::Command) -> Result<()>;
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
    fn apply(&self, worktree_path: &Path, _cmd: &mut tokio::process::Command) -> Result<()> {
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
    path.starts_with("/tmp") || path.starts_with("/var/tmp")
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
    if resolved.is_dir() { Some(resolved) } else { None }
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
    tracing::warn!(
        "sandbox: unsupported platform, falling back to FallbackSandbox heuristics"
    );
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
            444,                                  // SYS_landlock_create_ruleset
            std::ptr::null::<libc::c_void>(),     // attr = NULL
            0usize,                               // size = 0
            1i32,                                 // flags = LANDLOCK_CREATE_RULESET_VERSION
        )
    };
    ret > 0
}
