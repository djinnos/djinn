//! Ephemeral-clone cache for the chat subsystem.
//!
//! See `orientation-prompt-paste-into-gleaming-clover.md` §3. The chat
//! subsystem is user-scoped and globally multi-project; each
//! `(chat_session_id, project_id)` pair gets its own local `--shared`
//! clone of the project's bare mirror, rooted under `/var/tmp/djinn-chat/`.
//!
//! The cache is purely an on-disk workspace owner:
//!  - `acquire` returns a cached `Arc<ChatClone>` or runs `git clone
//!    --local --shared --branch <b>` from the mirror.
//!  - `release_session` drops every entry for a session (called on
//!    session end in commit 6).
//!  - `spawn_reaper` evicts entries idle longer than a configurable
//!    budget.
//!  - `boot_sweep` clears the root at server startup — clones are free
//!    to recreate and may be stale from a previous process lifetime.
//!
//! Path-traversal safety: both the session id and project id are
//! validated as UUIDs *before* any path concatenation. That check is
//! the only thing standing between an attacker-controlled tool arg
//! (`project_id`) and `/var/tmp/djinn-chat/<session>/../../etc/...`.
//!
//! Single-flight: a per-key mutex (same pattern as
//! `MirrorManager::lock_for`) guarantees two concurrent `acquire`s for
//! the same key don't race a duplicate `git clone`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use djinn_git::run_git_command;
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::mirror::MirrorManager;

/// Default on-disk root for chat clones.
///
/// `/var/tmp` (not `/tmp`) for the same reason as the agent sandbox:
/// `/tmp` is typically a RAM-backed tmpfs on modern distros and
/// running full working trees there eats the pod's memory budget.
pub const DEFAULT_CHAT_CLONE_ROOT: &str = "/var/tmp/djinn-chat";

/// Reaper idle budget — entries idle longer than this are candidates
/// for eviction. Matches the design doc's 30-minute window.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(30 * 60);

/// Reaper scan cadence — how often the reaper task wakes up.
pub const DEFAULT_REAPER_PERIOD: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Error)]
pub enum ChatCloneError {
    #[error("invalid id (must be UUID shape)")]
    InvalidId,

    #[error("invalid branch ref")]
    InvalidBranch,

    #[error("mirror for {0} does not exist")]
    MirrorMissing(String),

    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    #[error("git: {0}")]
    Git(String),
}

/// Cache key: `(chat_session_id, project_id)`.
type ChatCloneKey = (String, String);

/// One per-session-per-project ephemeral clone.
///
/// The `path` is `{root}/{chat_session_id}/{project_id}`. `last_used`
/// tracks wall-clock idle time for the reaper; callers update it via
/// [`ChatClone::touch`] on every access.
#[derive(Debug)]
pub struct ChatClone {
    pub path: PathBuf,
    pub project_id: String,
    last_used: tokio::sync::Mutex<Instant>,
}

impl ChatClone {
    fn new(path: PathBuf, project_id: String) -> Self {
        Self {
            path,
            project_id,
            last_used: tokio::sync::Mutex::new(Instant::now()),
        }
    }

    /// Bump the idle timer to `now`. Called on every cache hit.
    pub async fn touch(&self) {
        *self.last_used.lock().await = Instant::now();
    }

    /// Seconds since the last `touch`.
    pub async fn idle_for(&self) -> Duration {
        let last = *self.last_used.lock().await;
        Instant::now().saturating_duration_since(last)
    }
}

/// The cache itself.
pub struct ChatCloneCache {
    root: PathBuf,
    mirror_manager: Arc<MirrorManager>,
    entries: Mutex<HashMap<ChatCloneKey, Arc<ChatClone>>>,
    key_locks: Mutex<HashMap<ChatCloneKey, Arc<Mutex<()>>>>,
}

impl ChatCloneCache {
    /// Construct with a mirror manager and the on-disk root.
    ///
    /// Production callers pass `DEFAULT_CHAT_CLONE_ROOT`; tests pass a
    /// `tempfile::TempDir` path so they never touch `/var/tmp`.
    pub fn new(mirror_manager: Arc<MirrorManager>, root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            mirror_manager,
            entries: Mutex::new(HashMap::new()),
            key_locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    async fn key_lock(&self, key: &ChatCloneKey) -> Arc<Mutex<()>> {
        let mut guard = self.key_locks.lock().await;
        guard
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Resolve (or clone) the ephemeral workspace for
    /// `(chat_session_id, project_id)` at `branch`.
    ///
    /// Hot path: returns the cached `Arc<ChatClone>` after bumping
    /// `last_used`.
    ///
    /// Cold path: takes a per-key mutex, creates
    /// `{root}/{session}/{project}`, nukes any stale content left
    /// behind by a previous process, runs `git clone --local --shared
    /// --branch`, and inserts the new `Arc<ChatClone>` into the cache.
    ///
    /// Errors: both ids must be UUID-shaped (otherwise path traversal
    /// is trivial since `project_id` is tool-arg-sourced in chat);
    /// branch must match the permissive git-ref allowlist; the bare
    /// mirror must exist.
    pub async fn acquire(
        &self,
        chat_session_id: &str,
        project_id: &str,
        branch: &str,
    ) -> Result<Arc<ChatClone>, ChatCloneError> {
        if !is_uuid(chat_session_id) || !is_uuid(project_id) {
            return Err(ChatCloneError::InvalidId);
        }
        if !is_valid_branch_ref(branch) {
            return Err(ChatCloneError::InvalidBranch);
        }

        let key = (chat_session_id.to_string(), project_id.to_string());

        // Hot path: return cached + touch.
        if let Some(existing) = self.entries.lock().await.get(&key).cloned() {
            existing.touch().await;
            return Ok(existing);
        }

        // Single-flight per-key: two concurrent acquires for the same
        // key fall in behind one another and only one runs the clone.
        let lock = self.key_lock(&key).await;
        let _held = lock.lock().await;

        // Re-check under the lock.
        if let Some(existing) = self.entries.lock().await.get(&key).cloned() {
            existing.touch().await;
            return Ok(existing);
        }

        let mirror = self.mirror_manager.mirror_path(project_id);
        if !mirror.exists() {
            return Err(ChatCloneError::MirrorMissing(project_id.to_string()));
        }

        let target = self.root.join(chat_session_id).join(project_id);

        // If `target` exists as a file, refuse: something unrelated
        // is camped on the path and we don't know whose it is.
        if target.exists() {
            let meta = tokio::fs::metadata(&target).await?;
            if !meta.is_dir() {
                return Err(ChatCloneError::Io(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("{} exists but is not a directory", target.display()),
                )));
            }
            // Stale dir from a previous process or a failed earlier
            // attempt — nuke + recreate. Clones are cheap with full
            // mirrors.
            tokio::fs::remove_dir_all(&target).await?;
        }

        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        debug!(chat_session_id, project_id, branch, path = ?target, "cloning ephemeral chat workspace");

        // `run_git_command` uses `current_dir = target_parent` at the
        // point of exec, but git's `clone` takes absolute src + dst
        // args, so the cwd doesn't influence resolution. Pass the
        // parent as cwd (it exists; we just ensured it).
        let parent = target
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.root.clone());
        run_git_command(
            parent,
            vec![
                "clone".into(),
                "--local".into(),
                "--shared".into(),
                "--branch".into(),
                branch.to_string(),
                mirror.display().to_string(),
                target.display().to_string(),
            ],
        )
        .await
        .map_err(|e| match e {
            djinn_git::GitError::CommandFailed { stderr, .. } => {
                ChatCloneError::Git(format!("git clone --local: {stderr}"))
            }
            other => ChatCloneError::Git(format!("git clone --local: {other}")),
        })?;

        let clone = Arc::new(ChatClone::new(target, project_id.to_string()));
        self.entries.lock().await.insert(key, clone.clone());
        Ok(clone)
    }

    /// Evict every entry for `chat_session_id`, removing the
    /// on-disk directories too. Called on session end (commit 6
    /// wires it).
    pub async fn release_session(&self, chat_session_id: &str) {
        // Collect the keys we need to evict, then drop the lock before
        // any tokio::fs::remove_dir_all so filesystem slowness doesn't
        // hold the cache mutex.
        let to_evict: Vec<(ChatCloneKey, Arc<ChatClone>)> = {
            let mut entries = self.entries.lock().await;
            let keys: Vec<ChatCloneKey> = entries
                .keys()
                .filter(|(sid, _)| sid == chat_session_id)
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|k| entries.remove(&k).map(|v| (k, v)))
                .collect()
        };

        for (key, clone) in &to_evict {
            if let Err(e) = tokio::fs::remove_dir_all(&clone.path).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        chat_session_id = key.0,
                        project_id = key.1,
                        path = ?clone.path,
                        error = %e,
                        "release_session: failed to remove clone dir"
                    );
                }
            }
        }

        // Also drop the parent session directory if it's empty.
        let session_dir = self.root.join(chat_session_id);
        if let Ok(mut rd) = tokio::fs::read_dir(&session_dir).await {
            if rd.next_entry().await.ok().flatten().is_none() {
                let _ = tokio::fs::remove_dir(&session_dir).await;
            }
        }

        // Purge single-flight locks for this session too.
        let mut locks = self.key_locks.lock().await;
        locks.retain(|(sid, _), _| sid != chat_session_id);
    }

    /// Spawn a background task that every `period` scans the cache
    /// and evicts entries idle longer than `idle`.
    ///
    /// Deferred to commit 4: the live-PID check that should keep the
    /// reaper from ripping a directory out from under a running
    /// shell. The PID registry lives in the sandbox crate which
    /// doesn't exist yet in this commit. For now the reaper does a
    /// best-effort `rm -rf` and logs a warning on `EBUSY` etc.
    // TODO(commit 4): honour a per-(session, project) PID registry
    // from `djinn-agent`'s `ChatShellSandbox` and skip eviction when
    // a shell is live in the target directory.
    pub fn spawn_reaper(self: Arc<Self>, idle: Duration, period: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            // Skip the immediate first tick — we don't want to evict
            // the moment the reaper starts.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                self.reap_once(idle).await;
            }
        })
    }

    /// One reap pass — exposed for tests and for the background
    /// reaper loop. Evicts every entry idle longer than `idle`.
    pub async fn reap_once(&self, idle: Duration) {
        let candidates: Vec<(ChatCloneKey, Arc<ChatClone>)> = {
            let entries = self.entries.lock().await;
            let mut picked = Vec::new();
            for (k, v) in entries.iter() {
                if v.idle_for().await >= idle {
                    picked.push((k.clone(), v.clone()));
                }
            }
            picked
        };

        if candidates.is_empty() {
            return;
        }

        // Remove from the map first so new acquires won't hit the
        // doomed entry while we delete the directory.
        {
            let mut entries = self.entries.lock().await;
            for (k, _) in &candidates {
                entries.remove(k);
            }
        }

        for (key, clone) in &candidates {
            info!(
                chat_session_id = key.0,
                project_id = key.1,
                path = ?clone.path,
                "reaping idle chat clone"
            );
            if let Err(e) = tokio::fs::remove_dir_all(&clone.path).await {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        chat_session_id = key.0,
                        project_id = key.1,
                        path = ?clone.path,
                        error = %e,
                        "reaper: failed to remove clone dir (will retry on next pass)"
                    );
                }
            }
        }
    }

    /// Test-only accessor for cache size.
    #[cfg(test)]
    async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }
}

/// Startup sweep: remove `root` entirely. Clones are free to recreate
/// lazily on the next `acquire`, and anything left behind is from a
/// previous process lifetime.
pub async fn boot_sweep(root: &Path) {
    match tokio::fs::remove_dir_all(root).await {
        Ok(()) => info!(path = ?root, "boot_sweep: cleared chat clone root"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = ?root, "boot_sweep: root not present, nothing to do");
        }
        Err(e) => warn!(path = ?root, error = %e, "boot_sweep: failed to clear root"),
    }
}

/// UUID-shape check. 8-4-4-4-12 hex with hyphens, total 36 chars.
///
/// Strict enough to bar any `/` or `..` from a path segment while
/// being permissive enough to accept the output of every UUID
/// implementation we care about. We don't bother parsing into a
/// `uuid::Uuid` because the caller's argument is free-form text and
/// character-class rejection is all we need.
fn is_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    for (i, c) in s.chars().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if c != '-' {
                    return false;
                }
            }
            _ => {
                if !c.is_ascii_hexdigit() {
                    return false;
                }
                if c.is_ascii_uppercase() {
                    // Force lowercase-hex; keeps cache keys
                    // canonicalised.
                    return false;
                }
            }
        }
    }
    true
}

/// Permissive git-ref syntax check.
///
/// `git check-ref-format` is the canonical gate, but invoking git
/// per acquire is overkill for a ref we already got from
/// `projects.default_branch`. Replicate the subset that matters for
/// the `--branch` argument:
///   - non-empty, <= 255 bytes
///   - no ASCII control chars (including DEL, space, tab)
///   - no `..` sequence
///   - no `/.` or `./` sequences
///   - no leading `-` (or clone option-parsing treats it as a flag)
///   - no `:`, `?`, `*`, `[`, `\`, `^`, `~`, `@{`, `\\` — git's
///     `check-ref-format` forbidden set
fn is_valid_branch_ref(s: &str) -> bool {
    if s.is_empty() || s.len() > 255 {
        return false;
    }
    if s.starts_with('-') {
        return false;
    }
    if s.ends_with('/') || s.ends_with('.') || s.ends_with(".lock") {
        return false;
    }
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        // Bar control chars + DEL + space.
        if b < 0x20 || b == 0x7f || b == b' ' {
            return false;
        }
        match b {
            b':' | b'?' | b'*' | b'[' | b'\\' | b'^' | b'~' => return false,
            _ => {}
        }
        if b == b'.' && i + 1 < bytes.len() && bytes[i + 1] == b'.' {
            return false;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'.' {
            return false;
        }
        if b == b'.' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            return false;
        }
        if b == b'@' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // --- validators ---------------------------------------------------

    #[tokio::test]
    async fn acquire_rejects_non_uuid_session_id() {
        let tmp = TempDir::new().unwrap();
        let mirrors = TempDir::new().unwrap();
        let mm = Arc::new(MirrorManager::new(mirrors.path()));
        let cache = ChatCloneCache::new(mm, tmp.path());
        let err = cache
            .acquire(
                "../../../etc",
                "11111111-1111-1111-1111-111111111111",
                "main",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ChatCloneError::InvalidId), "got {err:?}");
    }

    #[tokio::test]
    async fn acquire_rejects_non_uuid_project_id() {
        let tmp = TempDir::new().unwrap();
        let mirrors = TempDir::new().unwrap();
        let mm = Arc::new(MirrorManager::new(mirrors.path()));
        let cache = ChatCloneCache::new(mm, tmp.path());
        let err = cache
            .acquire(
                "11111111-1111-1111-1111-111111111111",
                "../../../etc",
                "main",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ChatCloneError::InvalidId), "got {err:?}");
    }

    #[tokio::test]
    async fn acquire_rejects_bogus_branch() {
        let tmp = TempDir::new().unwrap();
        let mirrors = TempDir::new().unwrap();
        let mm = Arc::new(MirrorManager::new(mirrors.path()));
        let cache = ChatCloneCache::new(mm, tmp.path());
        let err = cache
            .acquire(
                "11111111-1111-1111-1111-111111111111",
                "22222222-2222-2222-2222-222222222222",
                "--upload-pack=evil",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ChatCloneError::InvalidBranch), "got {err:?}");
    }

    // --- real-clone tests --------------------------------------------

    /// Spin up a real bare mirror under `mm`'s root, backed by a
    /// freshly-initialised local source repo with one commit on
    /// `main`. Returns `(project_id, source_repo_path)`.
    async fn make_mirror(mm: &MirrorManager, project_id: &str) -> TempDir {
        let source = TempDir::new().unwrap();
        run_git_command(
            source.path().to_path_buf(),
            vec!["init".into(), "-b".into(), "main".into()],
        )
        .await
        .expect("git init");
        run_git_command(
            source.path().to_path_buf(),
            vec![
                "config".into(),
                "user.email".into(),
                "test@example.com".into(),
            ],
        )
        .await
        .expect("git config email");
        run_git_command(
            source.path().to_path_buf(),
            vec!["config".into(), "user.name".into(), "Test".into()],
        )
        .await
        .expect("git config name");
        tokio::fs::write(source.path().join("hello.txt"), b"hi\n")
            .await
            .expect("write file");
        run_git_command(
            source.path().to_path_buf(),
            vec!["add".into(), "hello.txt".into()],
        )
        .await
        .expect("git add");
        run_git_command(
            source.path().to_path_buf(),
            vec!["commit".into(), "-m".into(), "init".into()],
        )
        .await
        .expect("git commit");

        let url = source.path().display().to_string();
        mm.ensure_mirror(project_id, &url)
            .await
            .expect("ensure_mirror");
        source
    }

    #[tokio::test]
    async fn acquire_creates_clone_first_time() {
        let clone_root = TempDir::new().unwrap();
        let mirrors_root = TempDir::new().unwrap();
        let mm = Arc::new(MirrorManager::new(mirrors_root.path()));
        let project_id = "33333333-3333-3333-3333-333333333333";
        let _src = make_mirror(&mm, project_id).await;

        let cache = ChatCloneCache::new(mm, clone_root.path());
        let session_id = "44444444-4444-4444-4444-444444444444";
        let clone = cache
            .acquire(session_id, project_id, "main")
            .await
            .expect("acquire");

        assert_eq!(clone.project_id, project_id);
        assert_eq!(clone.path, clone_root.path().join(session_id).join(project_id));
        assert!(clone.path.is_dir(), "clone path should be a directory");
        assert!(
            clone.path.join("hello.txt").is_file(),
            "committed file should be present in the clone"
        );
    }

    #[tokio::test]
    async fn acquire_cached_second_call() {
        let clone_root = TempDir::new().unwrap();
        let mirrors_root = TempDir::new().unwrap();
        let mm = Arc::new(MirrorManager::new(mirrors_root.path()));
        let project_id = "55555555-5555-5555-5555-555555555555";
        let _src = make_mirror(&mm, project_id).await;

        let cache = ChatCloneCache::new(mm, clone_root.path());
        let session_id = "66666666-6666-6666-6666-666666666666";
        let first = cache
            .acquire(session_id, project_id, "main")
            .await
            .expect("first acquire");
        let second = cache
            .acquire(session_id, project_id, "main")
            .await
            .expect("second acquire");

        assert!(
            Arc::ptr_eq(&first, &second),
            "second acquire must return the cached Arc, not a new clone"
        );
        assert_eq!(cache.len().await, 1);
    }

    #[tokio::test]
    async fn release_session_evicts_and_deletes() {
        let clone_root = TempDir::new().unwrap();
        let mirrors_root = TempDir::new().unwrap();
        let mm = Arc::new(MirrorManager::new(mirrors_root.path()));
        let project_id = "77777777-7777-7777-7777-777777777777";
        let _src = make_mirror(&mm, project_id).await;

        let cache = ChatCloneCache::new(mm, clone_root.path());
        let session_id = "88888888-8888-8888-8888-888888888888";
        let clone = cache
            .acquire(session_id, project_id, "main")
            .await
            .expect("acquire");
        let path = clone.path.clone();
        assert!(path.is_dir());

        cache.release_session(session_id).await;

        assert_eq!(cache.len().await, 0, "cache should be empty");
        assert!(!path.exists(), "clone dir should be deleted");
    }

    #[tokio::test]
    async fn boot_sweep_clears_root() {
        let root = TempDir::new().unwrap();
        let dummy = root.path().join("leftover");
        tokio::fs::create_dir_all(dummy.join("inner"))
            .await
            .unwrap();
        tokio::fs::write(dummy.join("inner").join("f"), b"x")
            .await
            .unwrap();
        assert!(dummy.exists());

        boot_sweep(root.path()).await;

        assert!(
            !root.path().exists(),
            "boot_sweep should have removed the root"
        );
    }

    // --- reaper skipped in this commit --------------------------------
    // TODO(commit 4): once `ChatShellSandbox` lands with its PID
    // registry, add a reaper test that covers both "idle entry is
    // evicted after `idle` elapses" and "entry with a live shell is
    // preserved".
}
