//! Cross-Pod mutual exclusion for the semantic (SCIP) index phase.
//!
//! # The gap this closes
//!
//! `run_scip_index_command` and `ensure_canonical_graph` both take
//! `WarmContext::indexer_lock`, a `tokio::sync::Mutex` living in the process's
//! own address space. Since the standalone SCIP index became its own
//! Kubernetes Job (PR #2697) the two indexer fan-outs run in **different Pods,
//! in different processes**, so that mutex serialises nothing between them.
//!
//! It was observed doing exactly nothing on 2026-07-28: the warm Job and the
//! SCIP Job ran `rust-analyzer scip` concurrently on the same node, against the
//! same workspace at the same commit, peaking at 10.2 GB and 9.5 GB RSS
//! simultaneously. Both completed, so the cost was waste rather than an outage
//! — but the whole point of the split is that the warm Job *skips* the index
//! because the SCIP Job already filled the shared cache, and instead the node
//! paid for the same index twice.
//!
//! # What this is
//!
//! An advisory `flock(2)` on a file in the **shared `/cache` PVC**, keyed by
//! `(project_id, commit_sha)` — the tree identity that decides whether one
//! run's artifacts can serve the other, since the SCIP artifact cache key folds
//! in `source_hashes` over that same tree.
//!
//! The lock file lives under the SCIP cache root
//! (`$XDG_CACHE_HOME/djinn/scip-indexer`, i.e. `/cache/xdg/<project>/…` in every
//! djinn build Pod), which is deliberate: **the mutex lives on the same
//! filesystem as the resource it protects.** Two Pods that cannot see each
//! other's cache root have no shared cache to duplicate work into, and two Pods
//! that can see it can also see each other's lock.
//!
//! # Why a file lock and not a Postgres lease row
//!
//! Because of how it is released. This codebase has repeatedly been bitten by
//! coordination state that outlives its holder — occupancy rows that no reaper
//! reclaims, wedging the thing they were meant to protect. An advisory `flock`
//! has no reclamation problem to get wrong: the kernel drops it when the file
//! description closes, which happens on `close`, on `exit`, and on `SIGKILL`
//! alike. A crashed or OOM-killed indexer Pod releases its claim *immediately*,
//! with no expiry to tune, no heartbeat to miss and no janitor to schedule.
//! `.warm-locks/<project>.lock` (task `t6g0`) already uses this primitive for
//! the sibling problem of two warm Pods sharing one cargo base.
//!
//! A Postgres session advisory lock was the alternative. It is node-independent,
//! which this is not (see *Limitations*), but it would have to be held on a
//! dedicated connection for the ~59 minutes of an index, and a Pod that dies
//! with a half-open TCP connection keeps its lock until the server's keepalives
//! notice — which is precisely the "stale holder" failure mode being avoided.
//!
//! # Degradation is fail-open, in both directions
//!
//! * [`ClaimOutcome::Unavailable`] — no cache root, unwritable directory,
//!   `flock` unsupported by the filesystem, anything else. **Index anyway.**
//!   That is today's behaviour: wasteful when it duplicates, never wrong.
//! * [`ClaimOutcome::HeldByAnother`] — someone else is indexing this exact
//!   tree. The standalone SCIP Job may skip (it publishes nothing, so skipping
//!   cannot degrade the served graph). The warm path may **not** skip; it waits
//!   a bounded time for the holder to finish — which converts the duplicate
//!   index into a cache hit — and then proceeds regardless.
//!
//! No caller ever blocks indefinitely: [`claim_waiting`] takes a wait budget and
//! returns when it expires, whether or not the claim was obtained.
//!
//! # Limitations, stated plainly
//!
//! * `flock` is enforced per kernel. On the single-node production VPS, where
//!   both Pods run against the same local-path PVC, that is exactly the scope
//!   needed. On a multi-node cluster with a network filesystem whose `flock`
//!   is not cluster-coherent, two Pods on different nodes could both believe
//!   they hold the claim — degrading to today's duplicate-index behaviour, not
//!   to anything unsafe.
//! * The claim is keyed on the commit. Two indexers running at *different*
//!   commits do not contend and can still overlap in memory. Serialising those
//!   would mean making one of them wait for work that cannot help it; the
//!   scheduler-side gate (`djinn_k8s::scip_schedule`, which declines to dispatch
//!   while a warm Job is in flight) is what reduces that case.
//! * The identity written into the lock file is **diagnostic only**. Nothing
//!   reads it to decide anything, because a stale identity record must never be
//!   able to block or authorise an index. The `flock` is the whole protocol.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use djinn_core::clock::{Clock, SystemClock};

/// Directory under the SCIP cache root holding one lock file per claimed tree.
///
/// Dotted so the cache retention sweep — which identifies cache entries by the
/// presence of a `manifest.json` — never mistakes it for an evictable entry.
pub const CLAIM_DIR: &str = ".index-claims";

/// How long the warm path waits for another Pod's in-flight index of the same
/// tree before giving up and indexing inline itself.
///
/// Bounded by the warm Job's own deadline, and by the fact that the wait can be
/// a total loss. `warm_job_timeout_seconds` is 7200s and one complete measured
/// warm is 5442s end to end, so a warm that waits the full budget, gains
/// nothing and then does every phase itself must still fit:
/// `900 + 5442 = 6342 <= 7200`, with ~14 minutes of margin.
/// `the_claim_wait_budget_fits_inside_the_warm_job_deadline` in
/// `djinn_server::scip_index_watcher` proves that arithmetic against the real
/// config, from the crate that can see both constants.
///
/// When the wait *does* pay off it is strictly cheaper than the alternative:
/// waiting costs one idle process, while the duplicate it avoids costs a core
/// and ~10 GB of RSS for the same wall-clock window.
pub const DEFAULT_WAIT_SECONDS: u64 = 900;

/// Operator override for [`DEFAULT_WAIT_SECONDS`]. `0` disables waiting: the
/// warm path then behaves exactly as it did before this module existed.
pub const WAIT_ENV: &str = "DJINN_SCIP_CLAIM_WAIT_SECONDS";

/// Poll cadence while waiting for a claim.
///
/// Polling rather than a blocking `flock(LOCK_EX)`: a blocking acquisition
/// cannot be bounded without a signal, and an unbounded wait on a lock held by
/// another Pod is the stall this module is required not to introduce.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// An exclusive advisory claim on one `(project, commit)` semantic index.
///
/// Dropping it closes the descriptor, which releases the lock. Process death —
/// including `SIGKILL` and an OOM kill — has the same kernel-guaranteed effect,
/// which is the entire reason this is an `flock` and not a row.
#[derive(Debug)]
pub struct SemanticIndexClaim {
    file: File,
    path: PathBuf,
}

impl SemanticIndexClaim {
    /// Path of the lock file backing this claim.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for SemanticIndexClaim {
    fn drop(&mut self) {
        // Explicit unlock first so the release is observable even if the
        // descriptor somehow outlives this value; closing it releases the lock
        // regardless.
        let _ = flock(&self.file, libc::LOCK_UN);
    }
}

/// Result of trying to claim one tree's semantic index.
#[derive(Debug)]
pub enum ClaimOutcome {
    /// This process holds the claim until the guard is dropped.
    Held(SemanticIndexClaim),
    /// Another live process holds it. `holder` is a best-effort, purely
    /// diagnostic description of who — never trusted for a decision.
    HeldByAnother { holder: String },
    /// Coordination could not be established. Callers must fail open and index
    /// anyway; `reason` says what went wrong.
    Unavailable { reason: String },
}

impl ClaimOutcome {
    /// Closed-enum label, safe for logs and metrics.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Held(_) => "held",
            Self::HeldByAnother { .. } => "held_by_another",
            Self::Unavailable { .. } => "unavailable",
        }
    }

    /// True only when another live process demonstrably holds this exact tree's
    /// claim. This is the ONLY outcome that authorises a caller to skip
    /// indexing, and only for callers that publish nothing.
    #[must_use]
    pub fn is_held_by_another(&self) -> bool {
        matches!(self, Self::HeldByAnother { .. })
    }

    /// Take the guard, if this outcome carries one, so the caller can bind it
    /// for the duration of its indexer run.
    #[must_use]
    pub fn into_guard(self) -> Option<SemanticIndexClaim> {
        match self {
            Self::Held(claim) => Some(claim),
            _ => None,
        }
    }
}

/// Wait budget for [`claim_waiting`], from [`WAIT_ENV`] or the default.
#[must_use]
pub fn wait_budget_from_env() -> Duration {
    wait_budget_from(std::env::var(WAIT_ENV).ok().as_deref())
}

fn wait_budget_from(raw: Option<&str>) -> Duration {
    match raw.map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => match value.parse::<u64>() {
            Ok(seconds) => Duration::from_secs(seconds),
            Err(_) => Duration::from_secs(DEFAULT_WAIT_SECONDS),
        },
        None => Duration::from_secs(DEFAULT_WAIT_SECONDS),
    }
}

/// Lock-file path for one `(project, commit)` tree under `cache_root`.
///
/// The commit is the bulk of the identity; the project is included because a
/// development deployment can point several projects at one `XDG_CACHE_HOME`.
/// A digest of the raw project id is appended so two project ids that sanitize
/// to the same string still get different files.
#[must_use]
pub fn claim_path(cache_root: &Path, project_id: &str, commit_sha: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = hex::encode(Sha256::digest(project_id.as_bytes()));
    cache_root.join(CLAIM_DIR).join(format!(
        "{}-{}.{}.lock",
        sanitize(project_id, 48),
        &digest[..8],
        sanitize(commit_sha, 40),
    ))
}

fn sanitize(value: &str, max_len: usize) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(max_len)
        .collect();
    if cleaned.is_empty() {
        "unnamed".to_string()
    } else {
        cleaned
    }
}

/// Try to claim this tree's semantic index without waiting.
///
/// Never blocks. Every failure mode that is not "someone else holds it" is
/// reported as [`ClaimOutcome::Unavailable`], because a caller that cannot
/// coordinate must still index.
#[must_use]
pub fn try_claim(cache_root: &Path, project_id: &str, commit_sha: &str) -> ClaimOutcome {
    if commit_sha.trim().is_empty() {
        // Without a tree identity there is nothing to serialise on, and
        // serialising on the project alone would make two runs at different
        // commits wait for work that cannot help either of them.
        return ClaimOutcome::Unavailable {
            reason: "no commit sha to key the claim on".to_string(),
        };
    }
    let path = claim_path(cache_root, project_id, commit_sha);
    let Some(dir) = path.parent() else {
        return ClaimOutcome::Unavailable {
            reason: format!("claim path {} has no parent", path.display()),
        };
    };
    if let Err(error) = std::fs::create_dir_all(dir) {
        return ClaimOutcome::Unavailable {
            reason: format!("create {}: {error}", dir.display()),
        };
    }
    // `truncate(false)`: the file's *content* is diagnostic and must survive
    // being read by a contender; the lock is the flock, not the bytes.
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => {
            return ClaimOutcome::Unavailable {
                reason: format!("open {}: {error}", path.display()),
            };
        }
    };
    match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
        Ok(()) => {
            let claim = SemanticIndexClaim { file, path };
            record_holder(&claim, project_id, commit_sha);
            ClaimOutcome::Held(claim)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => ClaimOutcome::HeldByAnother {
            holder: read_holder(&path),
        },
        Err(error) => ClaimOutcome::Unavailable {
            reason: format!("flock {}: {error}", path.display()),
        },
    }
}

/// Claim this tree's semantic index, waiting up to `wait` for a current holder
/// to finish.
///
/// Returns as soon as the outcome is decided, and **always** returns by `wait`:
/// a holder that never finishes costs the caller its budget, not its run. A
/// holder that dies frees the claim within one [`POLL_INTERVAL`], because the
/// kernel — not a reaper — releases it.
///
/// The wait is worth taking only because the holder's product is shared: when
/// it finishes, the content-addressed SCIP cache holds artifacts for this exact
/// tree, so the waiter's own indexer fan-out becomes a cache hit per workspace
/// instead of a second 59-minute index.
pub async fn claim_waiting(
    cache_root: &Path,
    project_id: &str,
    commit_sha: &str,
    wait: Duration,
) -> ClaimOutcome {
    claim_waiting_with_poll(cache_root, project_id, commit_sha, wait, POLL_INTERVAL).await
}

/// [`claim_waiting`] with an explicit poll cadence. Exists so the waiting
/// behaviour can be tested at millisecond scale against the production loop
/// rather than against a re-implementation of it.
pub(crate) async fn claim_waiting_with_poll(
    cache_root: &Path,
    project_id: &str,
    commit_sha: &str,
    wait: Duration,
    poll: Duration,
) -> ClaimOutcome {
    let clock = SystemClock::new();
    let started = clock.now_instant();
    let mut outcome = try_claim(cache_root, project_id, commit_sha);
    if !outcome.is_held_by_another() || wait.is_zero() {
        return outcome;
    }
    tracing::info!(
        project_id,
        commit_sha,
        wait_secs = wait.as_secs(),
        holder = %holder_of(&outcome),
        "semantic index claim is held by another process for this exact tree; \
         waiting rather than running a duplicate index"
    );
    loop {
        let elapsed = started.elapsed();
        if elapsed >= wait {
            tracing::warn!(
                project_id,
                commit_sha,
                waited_secs = elapsed.as_secs(),
                holder = %holder_of(&outcome),
                "semantic index claim wait budget exhausted; indexing anyway \
                 (fail open — duplicated work, never a stalled or stale graph)"
            );
            return outcome;
        }
        let remaining = wait - elapsed;
        tokio::time::sleep(poll.min(remaining)).await;
        outcome = try_claim(cache_root, project_id, commit_sha);
        if !outcome.is_held_by_another() {
            tracing::info!(
                project_id,
                commit_sha,
                waited_secs = started.elapsed().as_secs(),
                outcome = outcome.reason(),
                "semantic index claim wait finished"
            );
            return outcome;
        }
    }
}

fn holder_of(outcome: &ClaimOutcome) -> &str {
    match outcome {
        ClaimOutcome::HeldByAnother { holder } => holder.as_str(),
        _ => "",
    }
}

/// Write a diagnostic record of who holds the claim. Best effort in every
/// respect: nothing reads it to make a decision, so a failure here is not worth
/// surfacing, and a stale record left by a dead holder cannot mislead anyone.
fn record_holder(claim: &SemanticIndexClaim, project_id: &str, commit_sha: &str) {
    let record = serde_json::json!({
        "pid": std::process::id(),
        "project_id": project_id,
        "commit_sha": commit_sha,
        "claimed_at": chrono::Utc::now().to_rfc3339(),
    })
    .to_string();
    // Truncate through the already-open descriptor: reopening could race with a
    // contender that has just acquired the lock after this process released it.
    let _ = claim.file.set_len(0);
    let _ = write_all_at_start(&claim.file, record.as_bytes());
}

fn write_all_at_start(file: &File, bytes: &[u8]) -> io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut handle = file;
    handle.seek(SeekFrom::Start(0))?;
    handle.write_all(bytes)?;
    handle.flush()
}

fn read_holder(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(contents) if !contents.trim().is_empty() => contents.trim().chars().take(300).collect(),
        _ => "unknown".to_string(),
    }
}

fn flock(file: &File, operation: libc::c_int) -> io::Result<()> {
    loop {
        // SAFETY: `file` owns a valid descriptor for the duration of the call;
        // flock takes no ownership and requires no aliasing guarantees.
        let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::thread;
    use tempfile::TempDir;

    const PROJECT: &str = "proj-xyz";
    const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER_COMMIT: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn root() -> TempDir {
        TempDir::new().expect("cache root")
    }

    /// **The property the in-process mutex could not provide.**
    ///
    /// Two independent acquisitions of the same tree's claim cannot both
    /// succeed. `flock` is scoped to the open file *description*, so two
    /// separate `open`s contend even inside one process — which is what makes
    /// this assertion about the same kernel primitive that separates two Pods.
    #[test]
    fn two_claims_on_the_same_tree_cannot_both_be_held() {
        let root = root();
        let first = try_claim(root.path(), PROJECT, COMMIT);
        assert!(
            matches!(first, ClaimOutcome::Held(_)),
            "first claim must be granted, got {first:?}"
        );

        let second = try_claim(root.path(), PROJECT, COMMIT);
        assert!(
            second.is_held_by_another(),
            "a second claim on the same tree must be refused, got {second:?}"
        );
        assert!(
            second.into_guard().is_none(),
            "a refused claim must carry no guard"
        );

        // Releasing hands the claim straight to the next caller — without this
        // the assertion above would also pass for a claim nobody can ever take.
        drop(first);
        assert!(matches!(
            try_claim(root.path(), PROJECT, COMMIT),
            ClaimOutcome::Held(_)
        ));
    }

    /// Different trees are different claims. Serialising them would make one
    /// run wait for an index whose artifacts cannot serve it — the cache key
    /// folds in `source_hashes`, so only the same tree helps.
    #[test]
    fn different_commits_and_projects_do_not_contend() {
        let root = root();
        let held = try_claim(root.path(), PROJECT, COMMIT);
        assert!(matches!(held, ClaimOutcome::Held(_)));
        assert!(matches!(
            try_claim(root.path(), PROJECT, OTHER_COMMIT),
            ClaimOutcome::Held(_)
        ));
        assert!(matches!(
            try_claim(root.path(), "other-project", COMMIT),
            ClaimOutcome::Held(_)
        ));
        assert_ne!(
            claim_path(root.path(), PROJECT, COMMIT),
            claim_path(root.path(), "other-project", COMMIT),
        );
    }

    /// **Stale state must not block.** A lock file left behind by a previous
    /// run — with a dead pid and an ancient timestamp inside it — is not a
    /// holder. The `flock` is the entire protocol; the recorded identity is
    /// diagnostic, and nothing may read it to decide anything.
    #[test]
    fn a_stale_lock_file_left_by_a_dead_holder_does_not_block() {
        let root = root();
        let path = claim_path(root.path(), PROJECT, COMMIT);
        std::fs::create_dir_all(path.parent().expect("claim dir")).expect("create claim dir");
        std::fs::write(
            &path,
            br#"{"pid":1,"claimed_at":"1999-01-01T00:00:00Z","project_id":"proj-xyz"}"#,
        )
        .expect("stale record");

        let outcome = try_claim(root.path(), PROJECT, COMMIT);
        assert!(
            matches!(outcome, ClaimOutcome::Held(_)),
            "a leftover lock FILE is not a held lock: {outcome:?}"
        );
        // …and the fresh holder replaced the stale record rather than leaving a
        // lie on disk for the next reader to log.
        let recorded = std::fs::read_to_string(&path).expect("read record");
        assert!(
            recorded.contains(&format!("\"pid\":{}", std::process::id())),
            "holder record was not refreshed: {recorded}"
        );
        assert!(!recorded.contains("1999-01-01"));
    }

    /// Coordination that cannot be established must fail **open**: index
    /// anyway. A caller that treats `Unavailable` as "someone else has it"
    /// would skip an index it was required to run.
    #[test]
    fn unusable_coordination_state_is_unavailable_not_contended() {
        // A cache root that cannot be created (a regular file where the
        // directory must go) is the general shape of "coordination is broken".
        let root = root();
        let blocked = root.path().join("not-a-dir");
        std::fs::write(&blocked, b"x").expect("write file");
        let outcome = try_claim(&blocked, PROJECT, COMMIT);
        assert!(
            matches!(outcome, ClaimOutcome::Unavailable { .. }),
            "unwritable claim directory must be Unavailable, got {outcome:?}"
        );
        assert!(
            !outcome.is_held_by_another(),
            "Unavailable must never authorise a skip"
        );

        // An unidentifiable tree is the same class of answer.
        let no_commit = try_claim(root.path(), PROJECT, "   ");
        assert!(matches!(no_commit, ClaimOutcome::Unavailable { .. }));
        assert!(!no_commit.is_held_by_another());
    }

    /// The waiter returns by its budget even when the holder never finishes,
    /// and it returns `HeldByAnother` — the caller then indexes anyway. An
    /// unbounded wait here would be a cross-Pod deadlock.
    #[tokio::test]
    async fn a_wait_against_a_holder_that_never_finishes_ends_at_its_budget() {
        let root = root();
        let _held = try_claim(root.path(), PROJECT, COMMIT);
        assert!(matches!(_held, ClaimOutcome::Held(_)));

        let clock = SystemClock::new();
        let started = clock.now_instant();
        let outcome = claim_waiting_with_poll(
            root.path(),
            PROJECT,
            COMMIT,
            Duration::from_millis(120),
            Duration::from_millis(20),
        )
        .await;
        let elapsed = started.elapsed();

        assert!(
            outcome.is_held_by_another(),
            "the holder never released, so the wait must end contended: {outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the wait must end at its budget, not at the holder's convenience \
             (took {elapsed:?})"
        );
    }

    /// The wait pays off when the holder finishes: the waiter takes the claim
    /// rather than starting a duplicate index.
    #[tokio::test]
    async fn a_wait_ends_early_when_the_holder_releases() {
        let root = root();
        let held = try_claim(root.path(), PROJECT, COMMIT);
        assert!(matches!(held, ClaimOutcome::Held(_)));
        let releaser = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            drop(held);
        });

        let outcome = claim_waiting_with_poll(
            root.path(),
            PROJECT,
            COMMIT,
            Duration::from_secs(30),
            Duration::from_millis(20),
        )
        .await;
        releaser.await.expect("releaser");
        assert!(
            matches!(outcome, ClaimOutcome::Held(_)),
            "a released claim must be granted to the waiter: {outcome:?}"
        );
    }

    /// A zero budget disables waiting entirely — the pre-existing behaviour,
    /// available as an operator lever.
    #[tokio::test]
    async fn a_zero_wait_budget_does_not_wait() {
        let root = root();
        let _held = try_claim(root.path(), PROJECT, COMMIT);
        let outcome = claim_waiting(root.path(), PROJECT, COMMIT, Duration::ZERO).await;
        assert!(outcome.is_held_by_another());
    }

    #[test]
    fn wait_budget_parses_and_falls_back() {
        assert_eq!(wait_budget_from(Some("42")), Duration::from_secs(42));
        assert_eq!(wait_budget_from(Some("0")), Duration::ZERO);
        assert_eq!(
            wait_budget_from(Some("nonsense")),
            Duration::from_secs(DEFAULT_WAIT_SECONDS)
        );
        assert_eq!(
            wait_budget_from(None),
            Duration::from_secs(DEFAULT_WAIT_SECONDS)
        );
    }

    // ---- the crash story, proved with a real process ------------------------

    const ROLE_ENV: &str = "DJINN_CLAIM_CONTRACT_ROLE";
    const ROOT_ENV: &str = "DJINN_CLAIM_CONTRACT_ROOT";

    /// **A holder that is SIGKILLed must not wedge the index.**
    ///
    /// This is the failure mode every expiry-based scheme has to engineer
    /// around, and the reason this claim is an `flock`: the kernel releases it
    /// when the process dies. Proved against a real, separately-scheduled
    /// process — a same-process guard drop would prove only that `Drop` runs,
    /// which is exactly what a `SIGKILL` denies you.
    #[test]
    fn a_holder_killed_mid_index_releases_its_claim_immediately() {
        let root = root();
        let mut holder = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "semantic_index_claim::tests::claim_contract_child",
                "--exact",
                "--nocapture",
            ])
            .env(ROLE_ENV, "holder")
            .env(ROOT_ENV, root.path())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn holder");

        wait_for(&root.path().join("holder-acquired"));
        let contended = try_claim(root.path(), PROJECT, COMMIT);
        assert!(
            contended.is_held_by_another(),
            "the child must really hold the claim before it is killed: {contended:?}"
        );

        holder.kill().expect("kill holder");
        holder.wait().expect("reap holder");

        // No unlock ran in the killed process. The kernel released it.
        let after = try_claim(root.path(), PROJECT, COMMIT);
        assert!(
            matches!(after, ClaimOutcome::Held(_)),
            "a claim held by a SIGKILLed process must be immediately \
             reclaimable — no expiry, no reaper: {after:?}"
        );
    }

    /// Re-exec'd child of the test above. Inert unless [`ROLE_ENV`] is set, so
    /// it is a no-op in a normal test run.
    #[test]
    fn claim_contract_child() {
        let Ok(role) = std::env::var(ROLE_ENV) else {
            return;
        };
        let root = PathBuf::from(std::env::var(ROOT_ENV).expect("claim root"));
        let claim = try_claim(&root, PROJECT, COMMIT);
        assert!(
            matches!(claim, ClaimOutcome::Held(_)),
            "child failed to acquire: {claim:?}"
        );
        std::fs::write(root.join(format!("{role}-acquired")), b"acquired").expect("marker");
        // Hold until killed. `std::mem::forget` is deliberate belt-and-braces:
        // nothing in this process may release the claim, so the parent's
        // assertion can only be satisfied by the kernel.
        std::mem::forget(claim);
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }

    #[allow(clippy::disallowed_methods)] // test-only spin-wait deadline
    fn wait_for(path: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
