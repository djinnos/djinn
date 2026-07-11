//! Unit tests for `preapproval_gate`. Split into a sibling file (referenced
//! via `#[path]` from `preapproval_gate.rs`) so the production module stays
//! under the Rust source-file size guard.

use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use djinn_core::events::EventBus;
use djinn_core::models::TaskStatus;
use djinn_db::repositories::task_run::CreateTaskRunParams;

use super::*;
use crate::ci_reproduction::RunnerOutput;

// ── Pure helpers ─────────────────────────────────────────────────────────

#[test]
fn kill_switch_defaults_on_and_off_list_disables() {
    assert!(gate_enabled_from_env_value(None), "unset = ON");
    assert!(gate_enabled_from_env_value(Some("")), "empty = ON");
    assert!(gate_enabled_from_env_value(Some("1")));
    assert!(gate_enabled_from_env_value(Some("true")));
    for off in ["0", "false", "no", "off", "OFF", " false "] {
        assert!(!gate_enabled_from_env_value(Some(off)), "{off} disables");
    }
}

#[test]
fn inline_exec_defaults_off() {
    assert!(!inline_exec_from_env_value(None));
    assert!(!inline_exec_from_env_value(Some("0")));
    assert!(!inline_exec_from_env_value(Some("false")));
    for on in ["1", "true", "yes", "on", " TRUE "] {
        assert!(inline_exec_from_env_value(Some(on)), "{on} enables");
    }
}

#[test]
fn path_scope_mirrors_ci_server_filter() {
    for server in [
        "server/crates/djinn-db/src/lib.rs",
        "server/src/main.rs",
        "server/crates/djinn-db/migrations_postgres/93_x.sql",
        "server/crates/djinn-core/Cargo.toml",
        "server/Cargo.toml",
        "server/.sqlx/query-abc.json",
        "scripts/check_boundaries.py",
        ".github/workflows/quality-gate.yml",
        "scripts/test-capability-boundaries.sh",
        "scripts/check-capability-boundaries.sh",
        "scripts/check-git-boundary.sh",
        "scripts/check-http-boundary.sh",
        "scripts/check-k8s-boundary.sh",
        "scripts/capability-boundary-allowlist.toml",
    ] {
        assert!(path_is_server_scope(server), "{server} is server-scope");
    }
    for non in [
        "docs/notes.md",
        "README.md",
        "ui/src/App.tsx",
        "server/crates/djinn-db/README.md",
        "",
    ] {
        assert!(!path_is_server_scope(non), "{non} is NOT server-scope");
    }
}

#[test]
fn changed_paths_docs_only_does_not_trigger_gate() {
    assert!(!changed_paths_trigger_server_gate(&[
        "docs/a.md".into(),
        "README.md".into()
    ]));
    assert!(changed_paths_trigger_server_gate(&[
        "docs/a.md".into(),
        "server/crates/x/src/lib.rs".into()
    ]));
}

#[test]
fn coverage_requires_all_checks_true() {
    let required = vec!["clippy_all_targets".to_string(), "size_guard".to_string()];
    let full = serde_json::json!({"clippy_all_targets": true, "size_guard": true});
    assert!(coverage_covers_required(Some(&full), &required));
    let partial = serde_json::json!({"clippy_all_targets": true});
    assert!(!coverage_covers_required(Some(&partial), &required));
    let a_false = serde_json::json!({"clippy_all_targets": true, "size_guard": false});
    assert!(!coverage_covers_required(Some(&a_false), &required));
    assert!(!coverage_covers_required(None, &required));
}

#[test]
fn check_set_scripts_exist() {
    // Guards against SERVER_CHECK_SET referencing a script CI removed.
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..");
    for rel in [
        "scripts/check-file-size.sh",
        "scripts/check-migrations-immutable.sh",
        "scripts/check-raw-sql-boundary.sh",
        ".github/workflows/quality-gate.yml",
        "scripts/test-capability-boundaries.sh",
        "scripts/check-git-boundary.sh",
        "scripts/check-http-boundary.sh",
        "scripts/check-k8s-boundary.sh",
        "scripts/premerge-gates.sh",
    ] {
        assert!(
            repo_root.join(rel).exists(),
            "referenced path {rel} must exist ({})",
            repo_root.join(rel).display()
        );
    }
    // The capability-boundary shared plumbing is sourced by the wrapper
    // scripts and is listed as a server-scope path, so it must exist too.
    assert!(
        repo_root
            .join("scripts/check-capability-boundaries.sh")
            .exists()
    );
    // The required check names are the coverage contract.
    assert_eq!(
        required_check_names(),
        vec![
            "clippy_all_targets",
            "size_guard",
            "migrations_guard",
            "raw_sql_boundary",
            "capability_boundaries",
            "sqlx_offline_cache",
            "test_target_build"
        ]
    );
}

#[test]
fn no_build_subset_is_the_deterministic_guards() {
    // The no-compile subset must be exactly the deterministic merge-queue
    // guards — the ones the gate can safely run inline in shadow mode.
    let names: Vec<&str> = no_build_checks().iter().map(|c| c.name).collect();
    assert_eq!(
        names,
        vec![
            "size_guard",
            "migrations_guard",
            "raw_sql_boundary",
            "capability_boundaries",
        ]
    );
    // Every no-build check is flagged accordingly, and every build check is
    // excluded from the subset.
    assert!(no_build_checks().iter().all(|c| !c.requires_build));
    for build_check in [
        "clippy_all_targets",
        "sqlx_offline_cache",
        "test_target_build",
    ] {
        assert!(
            !no_build_checks().iter().any(|c| c.name == build_check),
            "{build_check} must not be in the no-build subset"
        );
    }
}

#[test]
fn premerge_gates_script_covers_no_build_subset() {
    // Drift guard: scripts/premerge-gates.sh must run every no-compile
    // deterministic guard the Rust gate enforces, so the local reproduction
    // and the coordinator gate never diverge.
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..");
    let script = std::fs::read_to_string(repo_root.join("scripts/premerge-gates.sh"))
        .expect("read scripts/premerge-gates.sh");
    for check in no_build_checks() {
        assert!(
            script.contains(&format!("run_gate \"{}\"", check.name)),
            "premerge-gates.sh must run the '{}' gate",
            check.name
        );
    }
}

// ── Fake runner ──────────────────────────────────────────────────────────

/// Runs every command with a fixed exit code; counts invocations.
struct FakeRunner {
    exit: i32,
    calls: AtomicUsize,
}

impl FakeRunner {
    fn new(exit: i32) -> Self {
        Self {
            exit,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl CiReproductionRunner for FakeRunner {
    async fn run(
        &self,
        command: &str,
        _workdir: &Path,
        _timeout: Duration,
    ) -> Result<RunnerOutput, std::io::Error> {
        // The base-resolvability probe is infra, not a gate — always report a
        // resolvable base so the no-compile subset actually runs. It is not
        // counted as a check invocation.
        if command.contains("git rev-parse --verify origin/main") {
            return Ok(RunnerOutput {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(RunnerOutput {
            exit_code: self.exit,
            stdout: if self.exit == 0 {
                "ok".into()
            } else {
                "boom: something failed".into()
            },
            stderr: String::new(),
        })
    }
}

#[tokio::test]
async fn run_check_set_all_pass_and_all_fail() {
    let dir = tempfile::tempdir().unwrap();
    let pass = FakeRunner::new(0);
    let outcomes = run_checks(&pass, dir.path(), SERVER_CHECK_SET).await;
    assert_eq!(outcomes.len(), SERVER_CHECK_SET.len());
    assert!(outcomes.iter().all(|o| o.passed));
    assert_eq!(pass.calls.load(Ordering::SeqCst), SERVER_CHECK_SET.len());

    let fail = FakeRunner::new(1);
    let outcomes = run_checks(&fail, dir.path(), SERVER_CHECK_SET).await;
    assert!(outcomes.iter().all(|o| !o.passed));
    let feedback = format_gate_feedback(&outcomes);
    assert!(feedback.contains("clippy_all_targets"));
    assert!(feedback.contains("Pre-approval CI-grade verification gate failed"));
}

// ── DB-backed integration ────────────────────────────────────────────────

fn test_db() -> Database {
    Database::open_in_memory().unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Init a git worktree on `main` with a base commit, then add an untracked
/// file at `rel_path` (so the submission fingerprint sees a diff whose
/// changed_paths is exactly `[rel_path]`).
fn worktree_with_untracked(rel_path: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path();
    git(p, &["init", "-q"]);
    git(p, &["checkout", "-q", "-b", "main"]);
    git(p, &["config", "user.email", "t@test.local"]);
    git(p, &["config", "user.name", "T"]);
    std::fs::write(p.join("base.txt"), "base\n").unwrap();
    git(p, &["add", "base.txt"]);
    git(p, &["commit", "-q", "-m", "init"]);
    let full = p.join(rel_path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, "pub fn added() {}\n").unwrap();
    dir
}

async fn create_approved_task_with_run(db: &Database, workspace: &Path) -> (String, String) {
    let epic_repo = djinn_db::repositories::epic::EpicRepository::new(db.clone(), EventBus::noop());
    let epic = epic_repo.create("E", "", "", "", "", None).await.unwrap();

    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());
    let task = task_repo
        .create(&epic.id, "Task", "", "", "task", 0, "", Some("open"))
        .await
        .unwrap();
    // Land the task at `approved` (the seam the gate enforces).
    task_repo.set_status(&task.id, "approved").await.unwrap();

    let run_id = uuid::Uuid::now_v7().to_string();
    let ws = workspace.to_string_lossy().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &run_id,
            project_id: &epic.project_id,
            task_id: &task.id,
            trigger_type: "new_task",
            status: None,
            workspace_path: Some(&ws),
            mirror_ref: None,
        })
        .await
        .unwrap();
    (task.id, run_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn red_gate_blocks_pr_and_is_strike_free_kibj_shape() {
    let db = test_db();
    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let wt = worktree_with_untracked("server/crates/djinn-x/src/lib.rs");
    let (task_id, run_id) = create_approved_task_with_run(&db, wt.path()).await;
    let task = repo.get(&task_id).await.unwrap().unwrap();

    let runner = FakeRunner::new(1); // every CI-grade check fails
    let outcome = evaluate_and_enforce(&db, &repo, &task, true, true, &runner).await;

    assert!(
        matches!(outcome, PreApprovalGateOutcome::Blocked { .. }),
        "red gate must block, got {outcome:?}"
    );

    // Task returned to a worker round, strike-free.
    let after = repo.get(&task_id).await.unwrap().unwrap();
    assert_eq!(
        TaskStatus::parse(&after.status).unwrap(),
        TaskStatus::Open,
        "blocked task returns to open (worker round)"
    );
    assert_eq!(after.reopen_count, 0, "no reopen strike");
    assert_eq!(after.total_reopen_count, 0, "no total reopen strike");
    assert_eq!(after.intervention_count, 0, "no intervention counted");

    // Strike-free reviewer feedback delivered on the verification channel.
    let activity = repo
        .query_activity(djinn_db::ActivityQuery {
            task_id: Some(task_id.clone()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        activity.iter().any(|e| e.event_type == "comment"
            && e.actor_role == "verification"
            && e.payload
                .contains("Pre-approval CI-grade verification gate failed")),
        "a verification-role feedback comment must be present"
    );

    // A red verify_run row is persisted for the (task, fingerprint) cache.
    let runs = VerifyRunRepository::new(db.clone())
        .list_for_task_run(&run_id)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].result, VerifyResult::Fail.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_switch_off_is_noop() {
    let db = test_db();
    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let wt = worktree_with_untracked("server/crates/djinn-x/src/lib.rs");
    let (task_id, run_id) = create_approved_task_with_run(&db, wt.path()).await;
    let task = repo.get(&task_id).await.unwrap().unwrap();

    let runner = FakeRunner::new(1);
    let outcome = evaluate_and_enforce(&db, &repo, &task, false, true, &runner).await;

    assert_eq!(outcome, PreApprovalGateOutcome::Disabled);
    assert_eq!(runner.calls.load(Ordering::SeqCst), 0, "no checks run");
    // Behavior identical to today: task stays approved, nothing persisted.
    let after = repo.get(&task_id).await.unwrap().unwrap();
    assert_eq!(
        TaskStatus::parse(&after.status).unwrap(),
        TaskStatus::Approved
    );
    assert_eq!(after.reopen_count, 0);
    let runs = VerifyRunRepository::new(db.clone())
        .list_for_task_run(&run_id)
        .await
        .unwrap();
    assert!(runs.is_empty(), "no verify_run written when gate off");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docs_only_diff_bypasses_gate() {
    let db = test_db();
    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let wt = worktree_with_untracked("docs/notes.md");
    let (task_id, _run_id) = create_approved_task_with_run(&db, wt.path()).await;
    let task = repo.get(&task_id).await.unwrap().unwrap();

    let runner = FakeRunner::new(1); // would fail if it ran
    let outcome = evaluate_and_enforce(&db, &repo, &task, true, true, &runner).await;

    assert_eq!(outcome, PreApprovalGateOutcome::BypassedNonServer);
    assert_eq!(
        runner.calls.load(Ordering::SeqCst),
        0,
        "docs-only runs no checks"
    );
    let after = repo.get(&task_id).await.unwrap().unwrap();
    assert_eq!(
        TaskStatus::parse(&after.status).unwrap(),
        TaskStatus::Approved,
        "docs-only submission proceeds to PR unchanged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_green_verdict_short_circuits_and_does_not_rerun() {
    let db = test_db();
    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let wt = worktree_with_untracked("server/crates/djinn-x/src/lib.rs");
    let (task_id, _run_id) = create_approved_task_with_run(&db, wt.path()).await;
    let task = repo.get(&task_id).await.unwrap().unwrap();

    // First pass: green run persists a cache entry.
    let first = FakeRunner::new(0);
    let outcome1 = evaluate_and_enforce(&db, &repo, &task, true, true, &first).await;
    assert_eq!(outcome1, PreApprovalGateOutcome::PassRan);
    assert_eq!(first.calls.load(Ordering::SeqCst), SERVER_CHECK_SET.len());

    // Second pass, same (task, fingerprint): must hit cache, run 0 checks.
    let second = FakeRunner::new(1); // would fail if it ran
    let outcome2 = evaluate_and_enforce(&db, &repo, &task, true, true, &second).await;
    assert_eq!(
        outcome2,
        PreApprovalGateOutcome::PassCached,
        "unchanged fingerprint must be a cache hit"
    );
    assert_eq!(
        second.calls.load(Ordering::SeqCst),
        0,
        "cache hit must not re-run the check set"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_miss_shadow_mode_green_no_build_defers() {
    let db = test_db();
    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let wt = worktree_with_untracked("server/crates/djinn-x/src/lib.rs");
    let (task_id, run_id) = create_approved_task_with_run(&db, wt.path()).await;
    let task = repo.get(&task_id).await.unwrap().unwrap();

    // inline_exec = false, no cached verdict, no-compile guards all green →
    // defer the compile/DB checks to the verification pod (proceed).
    let runner = FakeRunner::new(0);
    let outcome = evaluate_and_enforce(&db, &repo, &task, true, false, &runner).await;
    assert_eq!(outcome, PreApprovalGateOutcome::DeferredNoVerdict);
    assert!(!outcome.should_block());
    // The no-compile subset ran inline (the base probe is not counted).
    assert_eq!(runner.calls.load(Ordering::SeqCst), no_build_checks().len());
    // A green no-compile subset must NOT persist a (partial) green run.
    let runs = VerifyRunRepository::new(db.clone())
        .list_for_task_run(&run_id)
        .await
        .unwrap();
    assert!(
        runs.is_empty(),
        "shadow-mode green no-compile subset must not persist a verify_run"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_miss_shadow_mode_red_no_build_blocks_strike_free() {
    let db = test_db();
    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let wt = worktree_with_untracked("server/crates/djinn-x/src/lib.rs");
    let (task_id, run_id) = create_approved_task_with_run(&db, wt.path()).await;
    let task = repo.get(&task_id).await.unwrap().unwrap();

    // inline_exec = false, no cached verdict, a no-compile guard fails → the
    // deterministic merge-queue offender is caught BEFORE the PR opens, even
    // in shadow mode. Strike-free: task returns to a worker round.
    let runner = FakeRunner::new(1);
    let outcome = evaluate_and_enforce(&db, &repo, &task, true, false, &runner).await;
    assert!(
        matches!(outcome, PreApprovalGateOutcome::Blocked { .. }),
        "red no-compile guard must block in shadow mode, got {outcome:?}"
    );
    // Only the no-compile subset ran (the compile/DB checks stay deferred).
    assert_eq!(runner.calls.load(Ordering::SeqCst), no_build_checks().len());

    let after = repo.get(&task_id).await.unwrap().unwrap();
    assert_eq!(
        TaskStatus::parse(&after.status).unwrap(),
        TaskStatus::Open,
        "blocked task returns to open (worker round)"
    );
    assert_eq!(after.reopen_count, 0, "no reopen strike");
    assert_eq!(after.total_reopen_count, 0, "no total reopen strike");
    assert_eq!(after.intervention_count, 0, "no intervention counted");

    // A red verify_run row is persisted for the (task, fingerprint) cache.
    let runs = VerifyRunRepository::new(db.clone())
        .list_for_task_run(&run_id)
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].result, VerifyResult::Fail.as_str());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cache_miss_shadow_mode_no_base_defers() {
    let db = test_db();
    let repo = TaskRepository::new(db.clone(), EventBus::noop());
    let wt = worktree_with_untracked("server/crates/djinn-x/src/lib.rs");
    let (task_id, _run_id) = create_approved_task_with_run(&db, wt.path()).await;
    let task = repo.get(&task_id).await.unwrap().unwrap();

    // A runner whose base probe reports NO resolvable base must fall back to
    // deferral, never running the changed-file guards (they would error on a
    // missing base and be misread as violations).
    struct NoBaseRunner {
        calls: AtomicUsize,
    }
    #[async_trait]
    impl CiReproductionRunner for NoBaseRunner {
        async fn run(
            &self,
            command: &str,
            _workdir: &Path,
            _timeout: Duration,
        ) -> Result<RunnerOutput, std::io::Error> {
            if command.contains("git rev-parse --verify origin/main") {
                // No base resolvable.
                return Ok(RunnerOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: String::new(),
                });
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(RunnerOutput {
                exit_code: 1,
                stdout: "should not run".into(),
                stderr: String::new(),
            })
        }
    }

    let runner = NoBaseRunner {
        calls: AtomicUsize::new(0),
    };
    let outcome = evaluate_and_enforce(&db, &repo, &task, true, false, &runner).await;
    assert_eq!(outcome, PreApprovalGateOutcome::DeferredNoVerdict);
    assert_eq!(
        runner.calls.load(Ordering::SeqCst),
        0,
        "no gate may run when the diff base is unresolvable"
    );
}
