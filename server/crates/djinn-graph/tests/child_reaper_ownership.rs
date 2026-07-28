//! The worker's subreaper must not steal a child that an in-process waiter
//! already owns.
//!
//! Regression test for the first production run of the standalone SCIP-index
//! Job, which failed with
//!
//! ```text
//! ensure index tree: spawn git rev-parse HEAD in /workspace/<project>
//! ```
//!
//! `djinn-agent-worker` sets `PR_SET_CHILD_SUBREAPER` and runs one
//! `waitpid(-1, WNOHANG)` loop. That call cannot tell an adopted orphan from a
//! child this process spawned and is already waiting on, so it could collect
//! the zombie of a `tokio::process` child first — after which tokio's own
//! `waitpid` returns `ECHILD` and a `git` command that ran perfectly is
//! reported as an I/O failure.
//!
//! Its own integration binary on purpose: the reaper installs a process-wide
//! child-status consumer, which would reap other tests' subprocesses if it
//! shared a binary with them.

#![cfg(target_os = "linux")]

use std::time::Duration;

use djinn_graph::child_reaper::{ChildReaper, worker_child_reaper};

/// Is `pid` a collectable zombie right now? Uses `WNOWAIT` so the check itself
/// never consumes the status it is looking for.
fn is_waitable(pid: u32) -> bool {
    // SAFETY: an all-zero siginfo_t is valid; the kernel leaves si_pid at 0 when
    // WNOHANG finds nothing, so it must start zeroed to be readable as "none".
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is valid writable memory for the duration of the call.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    // SAFETY: si_pid is initialized on a successful waitid.
    rc == 0 && unsafe { info.si_pid() } as u32 == pid
}

/// Wait until `pid` is a collectable zombie. Only meaningful for a PID the
/// reaper is bound to leave alone — an unclaimed child is collected so quickly
/// that it is never observed in this state.
async fn await_zombie(reaper: &ChildReaper, pid: u32) {
    for _ in 0..500 {
        if is_waitable(pid) {
            return;
        }
        assert!(
            !reaped_pids(reaper).contains(&pid),
            "the reaper collected pid {pid} out from under its owner — this is \
             the ECHILD bug: the owner's next wait will report \"No child \
             processes\" for a subprocess that ran fine"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("child {pid} never became collectable and was never reaped");
}

/// Wait until the reaper has recorded `pid` as an adopted child.
async fn await_reaped(reaper: &ChildReaper, pid: u32) -> bool {
    for _ in 0..500 {
        if reaped_pids(reaper).contains(&pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Let the reaper complete at least two full wait passes, so "it did not take
/// this pid" means it looked and declined rather than that it never looked.
fn let_the_reaper_look(reaper: &ChildReaper) {
    for _ in 0..2 {
        assert!(
            reaper.quiesce(Duration::from_secs(10)),
            "the reaper never completed a wait pass"
        );
    }
}

fn spawn_true() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("/bin/true");
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd
}

fn reaped_pids(reaper: &ChildReaper) -> Vec<u32> {
    reaper
        .adopted_children()
        .into_iter()
        .map(|record| record.status.pid)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_reaper_declines_an_owned_child_and_still_collects_an_unowned_one() {
    // SAFETY: changes this process's reparenting policy, exactly as the warm /
    // SCIP worker does at startup.
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) },
        0,
        "PR_SET_CHILD_SUBREAPER must be available for this test to mean anything"
    );
    let reaper = worker_child_reaper();

    // ---- owned: the reaper must leave this child's status for its waiter ----
    let (mut owned_child, ownership) = djinn_core::child_ownership::spawn_owned(
        || spawn_true().spawn(),
        tokio::process::Child::id,
    )
    .expect("spawn owned child");
    let owned_pid = ownership.pid().expect("owned child must have a pid");
    await_zombie(reaper, owned_pid).await;
    let_the_reaper_look(reaper);

    assert!(
        !reaped_pids(reaper).contains(&owned_pid),
        "the reaper collected pid {owned_pid} despite it being registered as \
         owned; its waiter will now see ECHILD"
    );
    // The assertion that actually matters: the owner can still collect it.
    let status = owned_child
        .wait()
        .await
        .expect("the owner must be able to wait on its own child");
    assert!(status.success(), "/bin/true should exit 0, got {status:?}");
    drop(ownership);

    // ---- unowned: the reaper must still do its job -------------------------
    //
    // Without this half, the test above would also pass against a reaper that
    // had simply stopped reaping anything at all.
    let mut unowned_child = spawn_true().spawn().expect("spawn unowned child");
    let unowned_pid = unowned_child.id().expect("unowned child must have a pid");

    assert!(
        await_reaped(reaper, unowned_pid).await,
        "the reaper must still collect a child no in-process waiter claimed \
         (adopted orphans are the whole reason it exists)"
    );
    assert!(
        unowned_child.wait().await.is_err(),
        "an unclaimed child IS taken by the reaper — if this ever starts \
         succeeding, the reaper stopped reaping and the owned-child assertion \
         above proves nothing"
    );
}
