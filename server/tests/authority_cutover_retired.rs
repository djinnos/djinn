//! HMI6 Item 5b: production validation makes the one-shot launcher-authority
//! cutover entrypoint permanently removable. This contract prevents the
//! rollback path from returning accidentally while preserving the permanent
//! fail-closed and administrative surfaces.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("server lives below the repository root")
        .to_path_buf()
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(repo_root().join(relative))
        .unwrap_or_else(|error| panic!("{relative} must remain readable: {error}"))
}

#[test]
fn the_one_shot_authority_cutover_entrypoint_stays_retired() {
    for relative in [
        "deploy/cutover/authority-cutover.sh",
        "server/src/authority_cutover.rs",
        "server/src/bin/authority_cutover.rs",
    ] {
        assert!(
            !repo_root().join(relative).exists(),
            "HMI6 Item 5b retired the one-shot surface; {relative} must not return"
        );
    }

    let manifest = read("server/Cargo.toml");
    assert!(!manifest.contains("name = \"authority-cutover\""));
    assert!(!manifest.contains("src/bin/authority_cutover.rs"));

    let library = read("server/src/lib.rs");
    assert!(!library.contains("pub mod authority_cutover;"));
    assert!(library.contains("pub mod task_run_resize_rollout;"));
}

#[test]
fn retirement_keeps_its_evidence_and_permanent_safety_surfaces() {
    let evidence = read("docs/deploy/RESIZE-V2-PROD-VALIDATED.md");
    for marker in [
        "250m -> 4 -> 250m",
        "approved for retirement",
        "PR #2893",
        "PR #2894",
    ] {
        assert!(
            evidence.contains(marker),
            "the authoritative production record must retain {marker:?}"
        );
    }

    let runbook = read("docs/deploy/launcher-authority-cutover.md");
    for command in [
        "djinn-server launcher-authority show",
        "djinn-server launcher-authority set resize-v2 --expected-epoch",
        "djinn-server launcher-authority set leaf-v1 --expected-epoch",
    ] {
        assert!(
            runbook.contains(command),
            "the durable authority runbook must document {command:?}"
        );
    }
    assert!(!runbook.contains("djinn-server admin launcher-authority"));

    for relative in [
        "deploy/preflight/cutover-preflight.sh",
        "docs/deploy/launcher-authority-cutover.md",
        "server/src/admin.rs",
        "server/src/task_run_resize_reconcile.rs",
    ] {
        assert!(
            repo_root().join(relative).is_file(),
            "retirement must preserve permanent safety surface {relative}"
        );
    }
}
