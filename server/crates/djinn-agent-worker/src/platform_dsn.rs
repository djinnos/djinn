//! The platform database DSN, taken out of the process environment at startup.
//!
//! # The exposure this closes (measured 2026-08-04)
//!
//! A task-run Pod's **worker container** is rendered with two Postgres DSNs:
//!
//! * `DATABASE_URL` / `TEST_POSTGRES_URL` — the pod-local `svc-postgres`
//!   sidecar on `127.0.0.1:5432`. Project-scoped, disposable, correct.
//! * `DJINN_DATABASE_URL` — the **platform** control-plane database. It holds
//!   `tasks`, `sessions`, `credentials` and `_sqlx_migrations`, and the role it
//!   authenticates as is DDL-capable.
//!
//! Every command the model runs — `bash -lc`, a `build.rs`, a test binary, an
//! npm `postinstall`, a language server — is a descendant of this process, so
//! every one of them inherited the second one. Nothing downstream of the worker
//! binary needs it: repository-controlled code that wants a database wants the
//! sidecar.
//!
//! # Why this is done here rather than at the spawn sites
//!
//! `unsetenv` in the parent is the only control that holds for spawn paths that
//! do not exist yet. The agent reaches a child process through the launcher
//! broker, through the launcher-free `bash -lc` fallback, through the
//! setup-hook runner, through git, through the LSP manager and through
//! project-configured MCP servers; each of those is a separate `Command`, and a
//! scrub attached to any subset of them is one new call site away from being
//! wrong again. A variable that is not in this process's environment cannot be
//! inherited by anything, and the `#[cfg(test)]` proof below asserts exactly
//! that against a child that applies no filtering at all.
//!
//! `djinn_cgroup_launcher::env`'s deny-set is the second, independent control:
//! it refuses the key at the broker boundary even if some other process in the
//! Pod holds the value.
//!
//! # Why the value survives (goxi launcher blocker 13)
//!
//! The worker *binary* legitimately needs the platform database:
//! `bootstrap_warm_database()` opens it, and `djinn_agent::context` builds the
//! durable invocation-lease authority — the thing that decides whether a build
//! may be lifted off the 250m unleased quota — over that handle and nothing
//! else. Blocker 13 was a wiring change that left that authority answering
//! `Unleased` for every invocation while it was armed, silently and with no
//! error. So the value is not dropped, it is *moved*: read once, before any
//! thread exists, into this module, and handed to `bootstrap_warm_database()`
//! from here.

use std::sync::RwLock;

/// The environment variable the Pod renders the platform DSN into.
pub const PLATFORM_DSN_ENV: &str = "DJINN_DATABASE_URL";

/// The DSN this process took out of its own environment, if it had one.
///
/// A `RwLock` rather than a `OnceLock` because tests take it repeatedly; the
/// production path writes it exactly once, from `main`, before the Tokio
/// runtime exists.
static PLATFORM_DSN: RwLock<Option<String>> = RwLock::new(None);

/// Move [`PLATFORM_DSN_ENV`] out of this process's environment and into
/// [`PLATFORM_DSN`].
///
/// # Safety
///
/// `std::env::remove_var` mutates a process-global that other threads may be
/// reading, so it is only sound while this process is single-threaded. The one
/// production caller is the first statement of `main`, before the Tokio runtime
/// is built and before any subcommand is dispatched — so every later
/// `bootstrap_warm_database()` call, in every subcommand, sees the stashed
/// value.
///
/// Idempotent: a second call with the variable already gone keeps the value
/// taken by the first.
pub fn take_from_environment() {
    let Some(value) = std::env::var_os(PLATFORM_DSN_ENV) else {
        return;
    };
    // SAFETY: called before the Tokio runtime is built and before any thread is
    // spawned; see the note above.
    unsafe { std::env::remove_var(PLATFORM_DSN_ENV) };
    let value = value.to_string_lossy().into_owned();
    *PLATFORM_DSN
        .write()
        .expect("platform DSN lock is never held across a panic") = Some(value);
}

/// The platform DSN taken by [`take_from_environment`], if any.
pub fn platform_dsn() -> Option<String> {
    PLATFORM_DSN
        .read()
        .expect("platform DSN lock is never held across a panic")
        .clone()
}

/// Return this process to the state `main` starts in: no rendered variable and
/// nothing taken. Test-only — production takes exactly once, from `main`.
///
/// # Safety
///
/// Mutates the process environment; callers must hold the serialising lock of
/// whichever test module they belong to.
#[cfg(test)]
pub(crate) fn clear_for_test() {
    // SAFETY: the caller holds its module's environment lock.
    unsafe { std::env::remove_var(PLATFORM_DSN_ENV) };
    *PLATFORM_DSN.write().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Serialises the tests in this module: they mutate the process
    /// environment, which every thread in the test binary shares.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn reset() {
        super::clear_for_test();
    }

    /// The regression, asserted on a child's ACTUAL environment.
    ///
    /// The child here is deliberately unfiltered — a bare
    /// `std::process::Command` with no allow-list, no `env_clear`, nothing.
    /// That is the point: it stands in for every spawn path in the worker,
    /// including the ones nobody has written yet. If this passes, none of them
    /// can see the platform DSN.
    #[test]
    fn no_child_of_this_process_can_observe_the_platform_dsn() {
        let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
        reset();
        // SAFETY: serialised by `_guard`.
        unsafe {
            std::env::set_var(
                PLATFORM_DSN_ENV,
                "postgres://djinn:hunter2@djinn-postgres.djinn.svc.cluster.local:5432/djinn",
            );
        }

        // Before: an ordinary child sees it. This half proves the test is
        // wired to something real rather than to an always-empty variable.
        let leaked = Command::new("/bin/sh")
            .arg("-c")
            .arg("printf %s \"${DJINN_DATABASE_URL-}\"")
            .output()
            .expect("spawn the probe child");
        assert_eq!(
            String::from_utf8_lossy(&leaked.stdout),
            "postgres://djinn:hunter2@djinn-postgres.djinn.svc.cluster.local:5432/djinn",
            "the probe must observe the variable before the scrub, or it proves nothing"
        );

        take_from_environment();

        let scrubbed = Command::new("/bin/sh")
            .arg("-c")
            .arg("printf %s \"${DJINN_DATABASE_URL-}\"")
            .output()
            .expect("spawn the probe child");
        assert_eq!(
            String::from_utf8_lossy(&scrubbed.stdout),
            "",
            "no child of the worker may observe the platform DSN"
        );
        // And nothing else in the environment leaks it either — a value scrub
        // that merely renamed the key would be the same defect.
        let dumped = Command::new("/bin/sh")
            .arg("-c")
            .arg("env")
            .output()
            .expect("spawn the probe child");
        assert!(
            !String::from_utf8_lossy(&dumped.stdout).contains("hunter2"),
            "the platform DSN must not survive under any key in the child environment"
        );

        reset();
    }

    /// The value is MOVED, not dropped: the worker binary must still be able to
    /// open the platform database. This is the half that keeps the
    /// invocation-lease authority alive (goxi launcher blocker 13); the
    /// end-to-end proof over a real database lives in `main.rs`'s test module.
    #[test]
    fn the_worker_keeps_the_value_it_took() {
        let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
        reset();
        // SAFETY: serialised by `_guard`.
        unsafe { std::env::set_var(PLATFORM_DSN_ENV, "postgres://u:p@host:5432/djinn") };

        take_from_environment();

        assert_eq!(
            platform_dsn().as_deref(),
            Some("postgres://u:p@host:5432/djinn"),
            "the DSN must survive the scrub for bootstrap_warm_database()"
        );
        assert!(
            std::env::var_os(PLATFORM_DSN_ENV).is_none(),
            "and must no longer be in this process's environment"
        );

        // Idempotent: a second take does not lose it.
        take_from_environment();
        assert_eq!(
            platform_dsn().as_deref(),
            Some("postgres://u:p@host:5432/djinn")
        );

        reset();
    }

    /// An absent variable stays absent — the local/non-pod run, where
    /// `bootstrap_warm_database()` must keep producing its own hard error
    /// rather than a confusing empty DSN.
    #[test]
    fn an_absent_variable_yields_no_dsn() {
        let _guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
        reset();
        take_from_environment();
        assert!(platform_dsn().is_none());
    }
}
