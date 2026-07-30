//! Linux-only adversarial regression for the real evidence executor sandbox.
//!
//! The fixture is deliberately data-driven so new policy bypass families cannot
//! quietly become validator-only tests. Every rejected request is sent to the
//! same `EvidenceSandbox::run` path used by `evidence_exec` after plan preflight.

#![cfg(target_os = "linux")]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use djinn_agent::test_helpers::test_tempdir;
use djinn_sandbox::{
    EVIDENCE_MAX_OUTPUT_BYTES, EvidenceProcessObserver, EvidenceRequest, EvidenceSandbox,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const FIXTURES: &str = include_str!("fixtures/evidence_exec_policy_cases.json");

#[derive(Debug, Deserialize)]
struct PolicyCase {
    id: String,
    allowed: bool,
    argv: Vec<String>,
    cwd: Option<String>,
}

fn host_git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .status()
        .expect("host git is available for fixture setup");
    assert!(status.success(), "git {args:?} failed with {status}");
}

fn output_git(dir: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("snapshot git command");
    assert!(output.status.success(), "git {args:?} failed: {output:?}");
    output.stdout
}

fn tree_bytes(root: &Path) -> Vec<u8> {
    fn visit(root: &Path, path: &Path, digest: &mut Sha256) {
        let mut entries = fs::read_dir(path)
            .expect("read fixture tree")
            .map(|entry| entry.expect("fixture entry"))
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let child = entry.path();
            let relative = child.strip_prefix(root).expect("relative fixture path");
            digest.update(relative.as_os_str().as_encoded_bytes());
            if child.is_dir() {
                digest.update(b"dir\0");
                visit(root, &child, digest);
            } else {
                digest.update(b"file\0");
                digest.update(fs::read(child).expect("fixture file bytes"));
            }
        }
    }
    let mut digest = Sha256::new();
    visit(root, root, &mut digest);
    digest.finalize().to_vec()
}

/// This test must be run in a Linux executor with user/network/PID namespace
/// support. If that capability is absent, `EvidenceSandbox` fails closed and
/// the real-child portion is intentionally not emulated by a weaker fallback.
#[tokio::test]
async fn evidence_exec_containment() {
    let cases: Vec<PolicyCase> = serde_json::from_str(FIXTURES).expect("checked-in policy corpus");
    assert!(cases.iter().any(|case| case.allowed));
    assert!(cases.iter().any(|case| !case.allowed));

    let temp = test_tempdir("evidence-exec-containment-");
    let remote = temp.path().join("fixture-remote.git");
    let clone = temp.path().join("clone");
    fs::create_dir(&remote).expect("remote directory");
    host_git(&remote, &["init", "--bare"]);
    host_git(temp.path(), &["clone", remote.to_str().expect("utf8 remote"), "clone"]);
    fs::write(clone.join("README.md"), "needle\nneedle\n").expect("fixture readme");
    fs::write(clone.join("package.json"), "{\"name\":\"evidence-fixture\"}\n")
        .expect("fixture JSON");
    host_git(&clone, &["add", "."]);
    host_git(&clone, &["-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-m", "fixture"]);
    host_git(&clone, &["push", "origin", "HEAD"]);

    let clone_before = tree_bytes(&clone);
    let clone_refs_before = output_git(&clone, &["show-ref", "--head"]);
    let remote_before = tree_bytes(&remote);
    let remote_refs_before = output_git(&remote, &["show-ref", "--head"]);
    let observer = EvidenceProcessObserver::new();
    let sandbox = EvidenceSandbox::with_process_observer(clone.clone(), observer.clone());

    let allowed = cases.iter().filter(|case| case.allowed).collect::<Vec<_>>();
    for case in &allowed {
        let result = sandbox
            .run(EvidenceRequest {
                argv: case.argv.clone(),
                cwd: case.cwd.as_ref().map(|cwd| clone.join(cwd)),
                timeout: Duration::from_secs(2),
                output_limit: EVIDENCE_MAX_OUTPUT_BYTES,
            })
            .await;
        assert!(result.is_ok(), "allowed case {} failed: {result:?}", case.id);
    }
    assert_eq!(
        observer.start_attempts(),
        allowed.len(),
        "every reviewed executable reaches the real isolated child boundary"
    );

    for case in cases.iter().filter(|case| !case.allowed) {
        let starts_before = observer.start_attempts();
        let result = sandbox
            .run(EvidenceRequest {
                argv: case.argv.clone(),
                cwd: case.cwd.as_ref().map(|cwd| clone.join(cwd)),
                timeout: Duration::from_secs(2),
                output_limit: EVIDENCE_MAX_OUTPUT_BYTES,
            })
            .await;
        assert!(result.is_err(), "forbidden case {} was accepted", case.id);
        assert_eq!(
            observer.start_attempts(),
            starts_before,
            "forbidden case {} reached a process/descendant launch boundary",
            case.id
        );
    }

    assert_eq!(tree_bytes(&clone), clone_before, "clone bytes changed");
    assert_eq!(output_git(&clone, &["show-ref", "--head"]), clone_refs_before, "clone refs changed");
    assert_eq!(tree_bytes(&remote), remote_before, "fixture remote bytes changed");
    assert_eq!(output_git(&remote, &["show-ref", "--head"]), remote_refs_before, "fixture remote refs changed");
}
