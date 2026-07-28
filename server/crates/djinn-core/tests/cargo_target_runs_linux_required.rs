//! The filesystem fixture contract is intentionally fail-closed off Linux.

// The two arms are `cfg`-selected rather than one arm asserting on `cfg!(...)`:
// `assert!(cfg!(target_os = "linux"), ..)` is a compile-time constant tested at
// run time (clippy::assertions_on_constants), which reads as a real assertion
// while being decided before the test ever runs. Behaviour is unchanged — the
// test exists on every platform and fails everywhere except Linux.

#[cfg(not(target_os = "linux"))]
#[test]
fn cargo_target_runs_fixture_contract_requires_linux_lstat_semantics() {
    panic!(
        "cargo-target-runs fixtures require deterministic Linux semantics: \
         lstat, hardlinks, and symlinks"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_target_runs_fixture_contract_requires_linux_lstat_semantics() {
    // Contract satisfied: Linux supplies the lstat/hardlink/symlink semantics
    // the cargo-target-runs fixtures are written against.
}
