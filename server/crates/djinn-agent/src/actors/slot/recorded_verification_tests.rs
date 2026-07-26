//! End-to-end coverage for the `recorded` final-verification evidence tier.
//!
//! These tests deliberately drive the real production path: the real project
//! config, the real `resolve_final_verification_for_task_run` resolver, the real
//! `coordinate_final_verification` coordinator, a real Postgres row, a real git
//! worktree, and a real spawned command. Nothing here injects a
//! `FinalVerificationExecutionEvidence` or hand-builds a `VerifyRun`.
//!
//! That is the point. The predecessor work shipped fourteen green acceptance
//! criteria against fixtures that supplied what production does not, and did
//! nothing in production. A test that hands the coordinator its answer proves
//! only that the coordinator can read.
//!
//! "The command did not run" is proven with a marker file OUTSIDE the worktree,
//! so observing it cannot itself perturb the fingerprint under test.

use std::path::{Path, PathBuf};

use djinn_core::events::EventBus;
use djinn_db::repositories::task_run::CreateTaskRunParams;
use djinn_db::{ImageRepository, ProjectRepository, TaskRunRepository};
use djinn_slot::final_verification::{
    FinalVerificationCoordinatorRequest, FinalVerificationRecordingOutcome,
    coordinate_final_verification,
};
use djinn_stack::environment::{
    EnvironmentConfig, FinalVerificationCommand, HermeticityDeclaration, VerificationEvidenceTier,
};
use tokio_util::sync::CancellationToken;

use crate::context::AgentContext;
use crate::test_helpers::{
    create_test_db, create_test_epic, create_test_project, create_test_task, test_tempdir,
};

const IMAGE_ID: &str = "recorded-verification-image";

async fn run_git(worktree: &Path, args: &[&str]) {
    djinn_git::run_git_command_in(worktree, args.iter().map(|arg| (*arg).to_owned()).collect())
        .await
        .unwrap_or_else(|error| panic!("git {args:?} failed: {error}"));
}

async fn identify(worktree: &Path) {
    run_git(worktree, &["config", "user.email", "test@example.com"]).await;
    run_git(worktree, &["config", "user.name", "Recorded Test"]).await;
}

/// A shared origin the way production has one: worker and reviewer runs are
/// separate Pods that only ever meet through the mirror.
async fn origin_repo() -> tempfile::TempDir {
    let origin = test_tempdir("recorded-origin-");
    run_git(origin.path(), &["init", "-b", "main"]).await;
    identify(origin.path()).await;
    std::fs::create_dir_all(origin.path().join("server/src")).expect("create tree");
    std::fs::write(origin.path().join("server/src/lib.rs"), "// base\n").expect("write source");
    std::fs::write(
        origin.path().join(".gitignore"),
        "server/target/\nui/node_modules/\n",
    )
    .expect("write gitignore");
    run_git(origin.path(), &["add", "."]).await;
    run_git(origin.path(), &["commit", "-m", "base"]).await;
    // The production mirror is a bare repository that accepts worker pushes to
    // the checked-out branch; a non-bare test origin refuses by default.
    run_git(
        origin.path(),
        &["config", "receive.denyCurrentBranch", "ignore"],
    )
    .await;
    origin
}

async fn clone_from(origin: &Path, prefix: &str) -> tempfile::TempDir {
    let clone = test_tempdir(prefix);
    run_git(
        clone.path(),
        &[
            "clone",
            origin.to_str().expect("origin path is UTF-8"),
            clone.path().to_str().expect("clone path is UTF-8"),
        ],
    )
    .await;
    identify(clone.path()).await;
    clone
}

/// Build the residue a worked-in Pod worktree accumulates and a fresh reviewer
/// clone never has: an ignored build directory and an ignored dependency tree.
fn add_build_residue(worktree: &Path) {
    let target = worktree.join("server/target/debug/deps");
    std::fs::create_dir_all(&target).expect("create target");
    std::fs::write(target.join("libdjinn.rlib"), b"warm artifact bytes").expect("write artifact");
    let modules = worktree.join("ui/node_modules/left-pad");
    std::fs::create_dir_all(&modules).expect("create node_modules");
    std::fs::write(modules.join("index.js"), b"module.exports = 1;").expect("write module");
}

struct Fixture {
    agent: AgentContext,
    project_id: String,
    task_id: String,
}

async fn fixture() -> Fixture {
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let images = ImageRepository::new(db.clone());
    images
        .create(IMAGE_ID, "recorded-verification", None, "{}")
        .await
        .expect("create catalog image");
    // The production resolver requires a ready, digest-pinned catalog image and
    // refuses to resolve without one; a test that skipped this would not be
    // exercising the production resolution boundary at all.
    images
        .mark_ready(
            IMAGE_ID,
            "ghcr.io/djinn/recorded-verification:test",
            Some(&format!("sha256:{}", "a".repeat(64))),
        )
        .await
        .expect("mark catalog image ready");
    images
        .set_project_image(&project.id, Some(IMAGE_ID))
        .await
        .expect("bind project image");
    Fixture {
        agent: agent_context_from_db_for_test(db),
        project_id: project.id,
        task_id: task.id,
    }
}

fn agent_context_from_db_for_test(db: djinn_db::Database) -> AgentContext {
    crate::test_helpers::agent_context_from_db(db, CancellationToken::new())
}

/// Insert a task_run the way a dispatch does: a fresh uuidv7 per run, its own
/// workspace path. Worker and reviewer runs of one task are separate rows.
async fn task_run(fx: &Fixture, worktree: &Path) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    let runs = TaskRunRepository::new(fx.agent.db.clone());
    runs.create(CreateTaskRunParams {
        id: &id,
        project_id: &fx.project_id,
        task_id: &fx.task_id,
        trigger_type: "dispatch",
        status: Some("running"),
        workspace_path: Some(worktree.to_str().expect("worktree path is UTF-8")),
        mirror_ref: None,
        dispatch_group_id: None,
    })
    .await
    .expect("create task run");
    runs.set_catalog_image_id(&id, IMAGE_ID)
        .await
        .expect("bind run image");
    id
}

/// The recorded plan under test. `marker` is appended to on every execution and
/// lives outside the worktree, so a run that was suppressed leaves it unchanged
/// without ever influencing the fingerprint.
fn recorded_plan(marker: &Path) -> EnvironmentConfig {
    let mut config = EnvironmentConfig::empty();
    let plan = &mut config.lifecycle.final_verification;
    plan.profile_id = "recorded-test".into();
    plan.profile_revision = 1;
    plan.evidence_tier = VerificationEvidenceTier::Recorded;
    plan.hermeticity = HermeticityDeclaration {
        hermetic: false,
        reusable: true,
        network_access: true,
    };
    plan.commands = vec![FinalVerificationCommand {
        check_id: "build".into(),
        executable: "/bin/sh".into(),
        argv: vec!["-c".into(), format!("echo ran >> {}", marker.display())],
        working_directory: String::new(),
        environment_names: vec!["PATH".into()],
        timeout_seconds: 60,
        descriptor_revision: 1,
    }];
    plan.required_checks = vec!["build".into()];
    plan.input_manifest.environment_names = vec!["PATH".into()];
    plan.output_only_globs = vec!["server/target/**".into(), "ui/node_modules/**".into()];
    config
}

async fn apply_config(fx: &Fixture, config: &EnvironmentConfig) {
    config
        .validate()
        .expect("recorded plan must satisfy environment validation");
    ProjectRepository::new(fx.agent.db.clone(), EventBus::noop())
        .set_environment_config(
            &fx.project_id,
            &serde_json::to_string(config).expect("serialize config"),
        )
        .await
        .expect("persist environment config");
}

async fn enable_reuse(fx: &Fixture) {
    djinn_db::repositories::settings::SettingsRepository::new(
        fx.agent.db.clone(),
        EventBus::noop(),
    )
    .set(
        &format!("project.{}.verify_run_reuse_enabled", fx.project_id),
        "true",
    )
    .await
    .expect("enable reuse");
}

/// Drive the coordinator through production callbacks with the observation
/// probe attached.
///
/// The probe is what DECLINES the `final_verification_outcome_for_test`
/// short-circuit. Without it, `AgentHostCallbacks` hands the coordinator a
/// synthetic `Stored` outcome carrying `"test-fingerprint"`, and every
/// assertion about outcomes passes while nothing at all executes. The marker
/// assertions below are what caught that; keep them.
async fn coordinate(fx: &Fixture, task_run_id: &str) -> FinalVerificationRecordingOutcome {
    let (callbacks, probe) =
        super::adapter::AgentHostCallbacks::dispatch_with_final_verification_probe(&fx.agent);
    let ctx = super::adapter::build_slot_context(&fx.agent, std::sync::Arc::new(callbacks), None);
    let outcome = coordinate_final_verification(
        FinalVerificationCoordinatorRequest {
            task_id: fx.task_id.clone(),
            task_run_id: task_run_id.to_owned(),
            cancellation: CancellationToken::new(),
        },
        &ctx,
    )
    .await;
    assert_eq!(
        probe
            .terminal_outcome_shortcuts
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "the synthetic test outcome must never decide these tests"
    );
    outcome
}

fn marker_runs(marker: &Path) -> usize {
    std::fs::read_to_string(marker).map_or(0, |text| text.lines().count())
}

fn marker_path(dir: &Path) -> PathBuf {
    dir.join("executions.log")
}

async fn fingerprint_of(fx: &Fixture, task_run_id: &str) -> String {
    let material =
        super::adapter::resolve_final_verification_for_task_run(&fx.agent.db, task_run_id)
            .await
            .expect("resolve material")
            .expect("configured plan");
    match djinn_git::compute_verification_input_fingerprint_with_config(
        &material.execution_request.worktree,
        &material.execution_request.fingerprint_config,
    )
    .await
    .expect("fingerprint computation")
    {
        djinn_git::VerificationInputFingerprint::Available(digest) => digest.fingerprint,
        djinn_git::VerificationInputFingerprint::Unavailable(reason) => {
            panic!("fingerprint unavailable: {reason}")
        }
    }
}

/// THE test the whole feature rests on.
///
/// A worker run works in a Pod worktree that accumulates ignored build output;
/// the reviewer run is a *separate* task_run in a *fresh clone* with none of it.
/// If those two fingerprint differently, reuse can never hit and the feature is
/// an elaborate no-op that still reports success. Nothing else here matters if
/// this fails.
#[tokio::test]
async fn worker_and_reviewer_clones_of_one_task_fingerprint_identically() {
    let origin = origin_repo().await;
    let fx = fixture().await;
    let scratch = test_tempdir("recorded-marker-");
    apply_config(&fx, &recorded_plan(&marker_path(scratch.path()))).await;

    // Worker: clone, author a change, commit it, and accumulate build residue.
    let worker_tree = clone_from(origin.path(), "recorded-worker-").await;
    std::fs::write(
        worker_tree.path().join("server/src/lib.rs"),
        "// authored\n",
    )
    .expect("author change");
    run_git(worker_tree.path(), &["add", "-A"]).await;
    run_git(worker_tree.path(), &["commit", "-m", "authored work"]).await;
    add_build_residue(worker_tree.path());
    let worker_run = task_run(&fx, worker_tree.path()).await;

    // Publish the branch the way a worker push does, so the reviewer can clone
    // exactly the same commit rather than a re-created one.
    run_git(worker_tree.path(), &["push", "origin", "main"]).await;

    // Reviewer: a brand new clone. No build residue whatsoever.
    let reviewer_tree = clone_from(origin.path(), "recorded-reviewer-").await;
    let reviewer_run = task_run(&fx, reviewer_tree.path()).await;
    assert!(
        !reviewer_tree.path().join("server/target").exists(),
        "the reviewer clone must genuinely lack the worker's build residue"
    );

    assert_eq!(
        fingerprint_of(&fx, &worker_run).await,
        fingerprint_of(&fx, &reviewer_run).await,
        "a worked-in worktree and a fresh clone of the same commit must agree, \
         or a reviewer can never reuse a worker's pass"
    );
}

/// Excluding build output must not delete it. The attested tier purges; a
/// recorded run reuses the very directory it excludes, so purging would convert
/// every run into a cold build — the exact failure this tier exists to remove.
#[tokio::test]
async fn recorded_fingerprint_excludes_build_output_without_deleting_it() {
    let origin = origin_repo().await;
    let fx = fixture().await;
    let scratch = test_tempdir("recorded-marker-");
    apply_config(&fx, &recorded_plan(&marker_path(scratch.path()))).await;

    let tree = clone_from(origin.path(), "recorded-preserve-").await;
    add_build_residue(tree.path());
    let run = task_run(&fx, tree.path()).await;
    let artifact = tree.path().join("server/target/debug/deps/libdjinn.rlib");

    let before = fingerprint_of(&fx, &run).await;
    assert!(artifact.exists(), "fingerprinting deleted the warm cache");

    // Changing excluded content must not move the fingerprint...
    std::fs::write(&artifact, b"recompiled artifact bytes").expect("rewrite artifact");
    assert_eq!(
        before,
        fingerprint_of(&fx, &run).await,
        "excluded build output must not participate in the fingerprint"
    );
    // ...and must still be on disk afterwards.
    assert_eq!(
        std::fs::read(&artifact).expect("artifact survives"),
        b"recompiled artifact bytes"
    );
}

/// The owner's requirement, end to end: the worker runs it, the reviewer skips
/// it. Proven by the command's own side effect, not by an outcome label alone.
#[tokio::test]
async fn worker_records_a_pass_that_a_reviewer_reuses_without_running_commands() {
    let origin = origin_repo().await;
    let fx = fixture().await;
    let scratch = test_tempdir("recorded-marker-");
    let marker = marker_path(scratch.path());
    apply_config(&fx, &recorded_plan(&marker)).await;
    enable_reuse(&fx).await;

    let worker_tree = clone_from(origin.path(), "recorded-worker-").await;
    add_build_residue(worker_tree.path());
    let worker_run = task_run(&fx, worker_tree.path()).await;

    let stored = coordinate(&fx, &worker_run).await;
    assert!(
        matches!(stored, FinalVerificationRecordingOutcome::Stored { .. }),
        "warm worker run must record a pass, got {stored:?}"
    );
    assert_eq!(marker_runs(&marker), 1, "the worker must actually compile");

    let reviewer_tree = clone_from(origin.path(), "recorded-reviewer-").await;
    let reviewer_run = task_run(&fx, reviewer_tree.path()).await;

    let reused = coordinate(&fx, &reviewer_run).await;
    assert!(
        matches!(reused, FinalVerificationRecordingOutcome::Reused { .. }),
        "an unchanged tree must reuse the worker's pass, got {reused:?}"
    );
    assert_eq!(
        marker_runs(&marker),
        1,
        "reuse must SKIP the command, not re-run it"
    );
}

/// Any real input change must invalidate — tracked, untracked, and ignored are
/// each checked independently, because they reach the fingerprint by three
/// different code paths (`ls-files -s`, `--others`, and `--others -i`).
#[tokio::test]
async fn any_input_change_invalidates_the_recorded_pass() {
    for (label, mutate) in [
        (
            "tracked",
            Box::new(|tree: &Path| {
                std::fs::write(tree.join("server/src/lib.rs"), "// edited\n").unwrap()
            }) as Box<dyn Fn(&Path)>,
        ),
        (
            "untracked",
            Box::new(|tree: &Path| std::fs::write(tree.join("scratch.txt"), "note\n").unwrap()),
        ),
        (
            "ignored-but-undeclared",
            Box::new(|tree: &Path| {
                // Ignored, yet NOT covered by an output-only glob, so it is a
                // real input. This is the case an over-broad glob would silently
                // erase — turning a no-op into a wrong answer.
                std::fs::write(tree.join(".gitignore"), "server/target/\nlocal.env\n").unwrap();
                std::fs::write(tree.join("local.env"), "SECRET=1\n").unwrap()
            }),
        ),
    ] {
        let origin = origin_repo().await;
        let fx = fixture().await;
        let scratch = test_tempdir("recorded-marker-");
        let marker = marker_path(scratch.path());
        apply_config(&fx, &recorded_plan(&marker)).await;
        enable_reuse(&fx).await;

        let first_tree = clone_from(origin.path(), "recorded-first-").await;
        let first_run = task_run(&fx, first_tree.path()).await;
        assert!(matches!(
            coordinate(&fx, &first_run).await,
            FinalVerificationRecordingOutcome::Stored { .. }
        ));
        assert_eq!(marker_runs(&marker), 1);

        let second_tree = clone_from(origin.path(), "recorded-second-").await;
        mutate(second_tree.path());
        let second_run = task_run(&fx, second_tree.path()).await;

        let outcome = coordinate(&fx, &second_run).await;
        assert!(
            matches!(outcome, FinalVerificationRecordingOutcome::Stored { .. }),
            "a {label} change must force a fresh run, got {outcome:?}"
        );
        assert_eq!(
            marker_runs(&marker),
            2,
            "a {label} change must re-execute the commands"
        );
    }
}

/// The rollout switch must be a genuine record-only mode: still runs, still
/// records, never suppresses. That property is what makes step 4 of the rollout
/// reversible, so it is asserted rather than assumed.
#[tokio::test]
async fn record_only_mode_records_every_run_and_reuses_none() {
    let origin = origin_repo().await;
    let fx = fixture().await;
    let scratch = test_tempdir("recorded-marker-");
    let marker = marker_path(scratch.path());
    apply_config(&fx, &recorded_plan(&marker)).await;
    // Deliberately do NOT enable the project switch.

    let first_tree = clone_from(origin.path(), "recorded-first-").await;
    let first_run = task_run(&fx, first_tree.path()).await;
    assert!(matches!(
        coordinate(&fx, &first_run).await,
        FinalVerificationRecordingOutcome::Stored { .. }
    ));

    let second_tree = clone_from(origin.path(), "recorded-second-").await;
    let second_run = task_run(&fx, second_tree.path()).await;
    let outcome = coordinate(&fx, &second_run).await;

    assert!(
        matches!(outcome, FinalVerificationRecordingOutcome::Stored { .. }),
        "record-only must re-run rather than reuse, got {outcome:?}"
    );
    assert_eq!(
        marker_runs(&marker),
        2,
        "record-only must still execute every run"
    );
}

/// `reusable: false` was not a kill-switch before this change: it influenced
/// only the identity digest, so a cache hit was still permitted. With the
/// project switch ON, an identical tree must still re-run.
#[tokio::test]
async fn a_plan_that_declares_itself_non_reusable_is_never_reused() {
    let origin = origin_repo().await;
    let fx = fixture().await;
    let scratch = test_tempdir("recorded-marker-");
    let marker = marker_path(scratch.path());
    let mut config = recorded_plan(&marker);
    config.lifecycle.final_verification.hermeticity.reusable = false;
    apply_config(&fx, &config).await;
    enable_reuse(&fx).await;

    let first_tree = clone_from(origin.path(), "recorded-first-").await;
    let first_run = task_run(&fx, first_tree.path()).await;
    assert!(matches!(
        coordinate(&fx, &first_run).await,
        FinalVerificationRecordingOutcome::Stored { .. }
    ));

    let second_tree = clone_from(origin.path(), "recorded-second-").await;
    let second_run = task_run(&fx, second_tree.path()).await;
    assert!(matches!(
        coordinate(&fx, &second_run).await,
        FinalVerificationRecordingOutcome::Stored { .. }
    ));
    assert_eq!(
        marker_runs(&marker),
        2,
        "a non-reusable plan must re-execute even on an identical tree"
    );
}

/// Defect regression: production hardcoded an empty allowlist, so any declared
/// name a catalog service did not export was rejected outright. A command that
/// reads a declared variable is the smallest honest proof it now arrives.
#[tokio::test]
async fn declared_environment_reaches_the_command() {
    let origin = origin_repo().await;
    let fx = fixture().await;
    let scratch = test_tempdir("recorded-marker-");
    let marker = marker_path(scratch.path());
    let mut config = recorded_plan(&marker);
    let plan = &mut config.lifecycle.final_verification;
    // HOME is set for every Pod process and for the test process alike.
    plan.input_manifest.environment_names = vec!["PATH".into(), "HOME".into()];
    plan.commands[0].environment_names = vec!["PATH".into(), "HOME".into()];
    plan.commands[0].argv = vec![
        "-c".into(),
        format!("test -n \"$HOME\" && echo ran >> {}", marker.display()),
    ];
    apply_config(&fx, &config).await;

    let tree = clone_from(origin.path(), "recorded-env-").await;
    let run = task_run(&fx, tree.path()).await;
    let outcome = coordinate(&fx, &run).await;

    assert!(
        matches!(outcome, FinalVerificationRecordingOutcome::Stored { .. }),
        "a declared environment name must reach the command, got {outcome:?}"
    );
    assert_eq!(marker_runs(&marker), 1, "outcome was {outcome:?}");
}

/// A volatile value differing between runs must NOT change the identity digest.
/// This is precisely what lets a reviewer (fresh `CARGO_TARGET_DIR`) reuse a
/// worker's pass; if it regressed, reuse would silently stop hitting.
#[tokio::test]
async fn volatile_environment_values_do_not_fracture_identity_across_runs() {
    let origin = origin_repo().await;
    let fx = fixture().await;
    let scratch = test_tempdir("recorded-marker-");
    let mut config = recorded_plan(&marker_path(scratch.path()));
    config
        .lifecycle
        .final_verification
        .input_manifest
        .volatile_environment_names = vec!["DJINN_RECORDED_VOLATILE".into()];
    apply_config(&fx, &config).await;

    let tree = clone_from(origin.path(), "recorded-volatile-").await;
    let run = task_run(&fx, tree.path()).await;

    let digest_for = |value: &str| {
        // SAFETY: nextest runs each test in its own process, and this mutation
        // happens before any resolver call in that process.
        unsafe { std::env::set_var("DJINN_RECORDED_VOLATILE", value) };
        async {
            let material =
                super::adapter::resolve_final_verification_for_task_run(&fx.agent.db, &run)
                    .await
                    .expect("resolve")
                    .expect("configured");
            let input = (material.execution_request.resolve_environment_identity)()
                .expect("identity input");
            let resolved = material.execution_request.volatile_environment.clone();
            let digest = djinn_core::canonical_verify::EnvironmentIdentityV1::derive(input)
                .expect("derive identity")
                .digest;
            (digest, resolved)
        }
    };

    let (first_digest, first_values) = digest_for("/cache/cargo-target-runs/run-one").await;
    let (second_digest, second_values) = digest_for("/cache/cargo-target-runs/run-two").await;

    assert_ne!(
        first_values, second_values,
        "the test must actually vary the volatile value"
    );
    assert_eq!(
        first_digest, second_digest,
        "a volatile value must not reach the identity digest"
    );
    unsafe { std::env::remove_var("DJINN_RECORDED_VOLATILE") };
}
