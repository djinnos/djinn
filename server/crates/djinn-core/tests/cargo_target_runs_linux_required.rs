//! The filesystem fixture contract is intentionally fail-closed off Linux.

#[cfg(not(unix))]
#[test]
fn cargo_target_runs_fixture_contract_requires_linux_lstat_semantics() {
    panic!("cargo-target-runs fixtures require Unix lstat, hardlinks, and symlinks");
}

#[cfg(unix)]
#[test]
fn cargo_target_runs_fixture_contract_requires_linux_lstat_semantics() {
    assert!(
        cfg!(target_os = "linux"),
        "fixtures require deterministic Linux semantics"
    );
}
