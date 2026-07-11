//! Pre-approval CI-grade verification gate (proposal `uv3p`, part A).
//!
//! Before an approved task's branch is pushed to GitHub (the
//! `approved → pr_draft → push_task_branch_to_github` path), this gate requires
//! a **green** verification result for the submitted diff fingerprint covering
//! the PR-level Quality-Gate check set for the touched paths — not clippy alone.
//! The 2026-07-04 park batch proved single-check gating is insufficient: strikes
//! came from the Server Size Guard, test-target builds, the sqlx offline-cache
//! check, and the Raw-SQL Boundary check as well as clippy.
//!
//! ## Single source of truth
//!
//! [`SERVER_CHECK_SET`] mirrors the PR-level `server`-scope jobs in
//! `.github/workflows/quality-gate.yml` (clippy `--all-targets`, the Server Size
//! Guard, the Raw-SQL Boundary check, the Server Capability Boundaries check,
//! the sqlx offline-cache check, and a test-target build). Each check delegates
//! to the **same repo script / cargo command** CI runs, so there is no second
//! command registry to drift. Whenever the workflow's server-scope check set
//! changes, update [`SERVER_CHECK_SET`] (a `check_set_scripts_exist` test
//! asserts the referenced scripts are present).
//!
//! ## Caching
//!
//! Results are cached by `(task, diff_fingerprint)` via the existing
//! `verify_runs` substrate (migration 87): an unchanged resubmission whose
//! fingerprint already has a green [`VerifyRunRecord`] covering the required
//! checks is allowed without recompiling.
//!
//! ## Strike-free feedback
//!
//! A red result never opens a PR, never increments `reopen_count` /
//! `total_reopen_count` / `intervention_count`, and never routes to a planner
//! park. It is delivered as reviewer-style feedback (a `verification`-role
//! `comment` activity, picked up by the worker prompt's `recent_feedback`
//! channel under the "Verification failure" label) and the task is returned to a
//! worker round via the strike-free `preapproval_verify_rejected`
//! (`approved → open`) transition.
//!
//! ## Kill-switch / bypass
//!
//! * `DJINN_PREAPPROVAL_VERIFY_GATE` — master switch, **default ON**. Set to
//!   `0`/`false`/`no`/`off` to disable the gate entirely (identical behavior to
//!   before this feature).
//! * `DJINN_PREAPPROVAL_VERIFY_GATE_INLINE_EXEC` — run the check set inline in
//!   the coordinator process, **default OFF**. When off (the default) the gate
//!   enforces off cached verify-run verdicts only and never blocks on a cache
//!   miss (the multi-minute compile is deferred to the verification pod — see
//!   the proposal's "land it in shadow mode first" risk note). Tests enable this
//!   with an injected [`CiReproductionRunner`] to exercise the full enforcement.
//! * Docs-only bypass: when none of the touched paths match the workflow's
//!   `server` change-detection filter, the server gate does not apply (mirrors
//!   `needs.changes.outputs.server`).

use std::path::Path;
use std::time::Duration;

use djinn_core::models::{Task, TaskStatus, TransitionAction, VerifyResult, VerifySource};
use djinn_db::Database;
use djinn_db::TaskRepository;
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::repositories::verify_run::{CreateVerifyRunParams, VerifyRunRepository};

use crate::ci_reproduction::{CiReproductionRunner, ShellRunner};

/// Env master switch for the pre-approval verification gate. Default ON.
const ENV_GATE: &str = "DJINN_PREAPPROVAL_VERIFY_GATE";
/// Env switch to run the check set inline in the coordinator process. Default
/// OFF — inline compile is deferred to the verification pod.
const ENV_INLINE_EXEC: &str = "DJINN_PREAPPROVAL_VERIFY_GATE_INLINE_EXEC";

/// Per-check timeout for the inline executor.
const CHECK_TIMEOUT: Duration = Duration::from_secs(600);

/// Max bytes of a failing check's captured output surfaced in feedback.
const MAX_CHECK_OUTPUT_BYTES: usize = 4 * 1024;

// ─── Shared check-set definition (mirrors quality-gate.yml `server` jobs) ─────

/// A single CI-grade check: a stable name and the exact shell command CI runs.
///
/// Commands are executed from the repository worktree root via `sh -c`, so
/// `cd server && …` and `./scripts/…` resolve exactly as they do in the
/// workflow. Keeping the command text identical to CI is the whole point — this
/// is the single source of truth the gate and CI share.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PreApprovalCheck {
    /// Stable coverage key (also the label used in `check_coverage`).
    pub name: &'static str,
    /// Exact shell command, run from the worktree root.
    pub command: &'static str,
    /// Whether this check needs a Cargo build and/or a live database.
    ///
    /// * `true` — the expensive checks (clippy, the sqlx offline-cache verify,
    ///   the test-target compile). These are deferred to the verification pod
    ///   unless inline execution is explicitly enabled.
    /// * `false` — pure shell/grep/git guards (the Size Guard, the Migrations
    ///   Guard, the Raw-SQL Boundary, the capability boundaries) that finish in
    ///   milliseconds against the worktree. These run inline **even in shadow
    ///   mode** so the deterministic merge-queue offenders are caught before a
    ///   PR is opened rather than post-approval in the merge queue.
    pub requires_build: bool,
}

/// The PR-level `server`-scope Quality-Gate check set.
///
/// Mirrors `.github/workflows/quality-gate.yml`: `server-clippy`,
/// `server-size-guard`, `server-migrations-guard`, `server-raw-sql-boundary`,
/// `server-capability-boundaries`, `server-sqlx-cache`, and a test-target build
/// (the `warm-cache-test` / merge-queue `server-test` compile).
/// `SQLX_OFFLINE=true` matches the workflow's offline consumers.
///
/// `requires_build` splits the set into the no-compile deterministic guards
/// (always run inline, even in shadow mode) and the compile/DB-heavy checks
/// (deferred to the verification pod unless inline execution is enabled). The
/// repo-provided `scripts/premerge-gates.sh` runs the same no-compile subset for
/// local reproduction; a drift-guard test keeps the two in lockstep.
pub(crate) const SERVER_CHECK_SET: &[PreApprovalCheck] = &[
    PreApprovalCheck {
        name: "clippy_all_targets",
        command: "cd server && SQLX_OFFLINE=true cargo clippy --all-targets --features qdrant -- -D warnings",
        requires_build: true,
    },
    PreApprovalCheck {
        name: "size_guard",
        // Changed-file mode, same as the workflow's server-size-guard job. The
        // base SHA is resolved from the merge-base against origin/main (then
        // main) with a HEAD fallback, mirroring the workflow's BASE_SHA logic.
        command: "BASE=$(git merge-base origin/main HEAD 2>/dev/null || git merge-base main HEAD 2>/dev/null || git rev-parse HEAD); git diff --name-only --diff-filter=AMR \"$BASE\"...HEAD | ./scripts/check-file-size.sh --files-from-stdin",
        requires_build: false,
    },
    PreApprovalCheck {
        name: "migrations_guard",
        // Applied sqlx migrations are immutable — editing/renaming/deleting one
        // that exists on the base ref breaks the migrate init container on every
        // existing database, so it passes a fresh CI DB and only detonates in
        // the merge queue. Mirrors the workflow's server-migrations-guard job.
        // BASE_SHA is resolved with the same merge-base + HEAD fallback as the
        // size guard, so a base-less checkout diffs HEAD...HEAD (empty) and never
        // false-blocks.
        command: "BASE=$(git merge-base origin/main HEAD 2>/dev/null || git merge-base main HEAD 2>/dev/null || git rev-parse HEAD); BASE_SHA=\"$BASE\" ./scripts/check-migrations-immutable.sh",
        requires_build: false,
    },
    PreApprovalCheck {
        name: "raw_sql_boundary",
        command: "./scripts/check-raw-sql-boundary.sh",
        requires_build: false,
    },
    PreApprovalCheck {
        name: "capability_boundaries",
        // Self-tests first, then the live per-capability detector scripts. The
        // shell self-tests run the detectors against synthetic fixtures so a
        // detector or allowlist regression fails loudly before the live scan.
        // No BASE_SHA is injected here; the scripts fall back to origin/main for
        // the diff, keeping the command deterministic and repository-local.
        command: "./scripts/test-capability-boundaries.sh && ./scripts/check-git-boundary.sh && ./scripts/check-http-boundary.sh && ./scripts/check-k8s-boundary.sh",
        requires_build: false,
    },
    PreApprovalCheck {
        name: "sqlx_offline_cache",
        command: "make sqlx-verify",
        requires_build: true,
    },
    PreApprovalCheck {
        name: "test_target_build",
        command: "cd server && SQLX_OFFLINE=true cargo nextest run --workspace --all-targets --features qdrant --no-run",
        requires_build: true,
    },
];

/// The no-compile deterministic guards from [`SERVER_CHECK_SET`] — the Size
/// Guard, Migrations Guard, Raw-SQL Boundary, and capability boundaries. These
/// need no Cargo build and no database, so the gate runs them inline even when
/// inline execution is disabled (shadow mode) to catch the deterministic
/// merge-queue offenders before a PR is opened.
pub(crate) fn no_build_checks() -> Vec<PreApprovalCheck> {
    SERVER_CHECK_SET
        .iter()
        .filter(|c| !c.requires_build)
        .copied()
        .collect()
}

/// Check names required for a cached green verdict to be considered complete
/// coverage.
pub(crate) fn required_check_names() -> Vec<String> {
    SERVER_CHECK_SET
        .iter()
        .map(|c| c.name.to_string())
        .collect()
}

// ─── Kill-switch parsing (pure, testable) ─────────────────────────────────────

/// Pure default-ON parser: unset/empty → ON; `0|false|no|off` → OFF.
pub(crate) fn gate_enabled_from_env_value(value: Option<&str>) -> bool {
    match value {
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        None => true,
    }
}

/// Pure default-OFF parser: only `1|true|yes|on` → ON.
pub(crate) fn inline_exec_from_env_value(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Read the master kill-switch from the environment (default ON).
pub(crate) fn gate_enabled() -> bool {
    gate_enabled_from_env_value(std::env::var(ENV_GATE).ok().as_deref())
}

/// Read the inline-execution switch from the environment (default OFF).
pub(crate) fn inline_exec_enabled() -> bool {
    inline_exec_from_env_value(std::env::var(ENV_INLINE_EXEC).ok().as_deref())
}

// ─── Touched-path scope classifier (mirrors the CI `changes` filter) ──────────

/// Whether a changed path selects the workflow's `server` gate.
///
/// Mirrors the `server:` filter in `.github/workflows/quality-gate.yml` (the
/// `changes` job's `dorny/paths-filter`). A diff that touches none of these is
/// docs-only / non-server and the server check set does not apply.
pub(crate) fn path_is_server_scope(path: &str) -> bool {
    let p = path.trim();
    if p.is_empty() {
        return false;
    }
    // Exact-file rules from the workflow.
    const EXACT: &[&str] = &[
        ".djinn/skills.json",
        "server/Cargo.toml",
        "server/Cargo.lock",
        "server/deny.toml",
        "server/boundary_rules.toml",
        "scripts/check_boundaries.py",
        "server/rust-toolchain.toml",
        ".github/workflows/quality-gate.yml",
        "scripts/test-capability-boundaries.sh",
        "scripts/check-capability-boundaries.sh",
        "scripts/check-git-boundary.sh",
        "scripts/check-http-boundary.sh",
        "scripts/check-k8s-boundary.sh",
        "scripts/capability-boundary-allowlist.toml",
    ];
    if EXACT.contains(&p) {
        return true;
    }
    // Prefix / glob rules from the workflow.
    if p.starts_with(".claude/skills/")
        || p.starts_with(".opencode/skills/")
        || p.starts_with(".djinn/skills/")
        || p.starts_with("server/.cargo/")
        || p.starts_with("server/.config/")
        || p.starts_with("server/.sqlx/")
        || p.starts_with("server/src/")
    {
        return true;
    }
    // `server/crates/**/Cargo.toml`, `server/crates/**/migrations_postgres/**`,
    // and `server/crates/**/*.rs`.
    if p.starts_with("server/crates/") {
        if p.ends_with("Cargo.toml") || p.ends_with(".rs") {
            return true;
        }
        if p.contains("/migrations_postgres/") {
            return true;
        }
    }
    false
}

/// Whether the touched paths trigger the server gate. `false` (docs-only /
/// non-server) means the gate does not apply.
pub(crate) fn changed_paths_trigger_server_gate(changed_paths: &[String]) -> bool {
    changed_paths.iter().any(|p| path_is_server_scope(p))
}

// ─── Executor ─────────────────────────────────────────────────────────────────

/// Outcome of a single check-set command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CheckOutcome {
    pub name: String,
    pub passed: bool,
    pub exit_code: i32,
    /// Bounded tail of combined stdout+stderr (only populated on failure).
    pub output: String,
}

/// Whether the worktree has a diff base the changed-file guards can resolve
/// (`origin/main` or `main`). The no-compile guards diff against it; if neither
/// ref exists we must not run them (they would error on "no base" and be
/// misread as a violation), so the caller defers instead of false-blocking.
pub(crate) async fn base_ref_resolvable(runner: &dyn CiReproductionRunner, workdir: &Path) -> bool {
    runner
        .run(
            "git rev-parse --verify origin/main >/dev/null 2>&1 || git rev-parse --verify main >/dev/null 2>&1",
            workdir,
            Duration::from_secs(30),
        )
        .await
        .map(|o| o.exit_code == 0)
        .unwrap_or(false)
}

/// Run an explicit list of checks in `workdir`, stopping-nothing (all run so
/// feedback lists every failure, matching CI's `fail-fast: false`).
pub(crate) async fn run_checks(
    runner: &dyn CiReproductionRunner,
    workdir: &Path,
    checks: &[PreApprovalCheck],
) -> Vec<CheckOutcome> {
    let mut outcomes = Vec::with_capacity(checks.len());
    for check in checks {
        let outcome = match runner.run(check.command, workdir, CHECK_TIMEOUT).await {
            Ok(out) if out.exit_code == 0 => CheckOutcome {
                name: check.name.to_string(),
                passed: true,
                exit_code: 0,
                output: String::new(),
            },
            Ok(out) => {
                let mut combined = out.stdout;
                if !out.stderr.is_empty() {
                    if !combined.is_empty() {
                        combined.push_str("\n--- stderr ---\n");
                    }
                    combined.push_str(&out.stderr);
                }
                CheckOutcome {
                    name: check.name.to_string(),
                    passed: false,
                    exit_code: out.exit_code,
                    output: tail_bytes(&combined, MAX_CHECK_OUTPUT_BYTES),
                }
            }
            Err(e) => CheckOutcome {
                name: check.name.to_string(),
                // Infra failure to even spawn the command is treated as a
                // non-pass (the check did not demonstrably pass), but it is
                // captured distinctly so feedback shows it was an execution
                // error, not a lint/test failure.
                passed: false,
                exit_code: -1,
                output: format!("check could not run: {e}"),
            },
        };
        outcomes.push(outcome);
    }
    outcomes
}

/// Build the `check_coverage` JSON object (`{name: passed_bool, …}`) recorded on
/// the `verify_runs` row.
pub(crate) fn check_coverage_json(outcomes: &[CheckOutcome]) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> = outcomes
        .iter()
        .map(|o| (o.name.clone(), serde_json::Value::Bool(o.passed)))
        .collect();
    serde_json::Value::Object(map)
}

/// Whether a cached run's `check_coverage` covers every required check with a
/// `true` value. A cached green run missing a required check is not a cache hit.
pub(crate) fn coverage_covers_required(
    coverage: Option<&serde_json::Value>,
    required: &[String],
) -> bool {
    let map = match coverage {
        Some(serde_json::Value::Object(m)) => m,
        _ => return required.is_empty(),
    };
    required
        .iter()
        .all(|k| map.get(k).and_then(|v| v.as_bool()).unwrap_or(false))
}

/// Format the strike-free reviewer-style feedback body for a red gate result.
pub(crate) fn format_gate_feedback(outcomes: &[CheckOutcome]) -> String {
    let failed: Vec<&CheckOutcome> = outcomes.iter().filter(|o| !o.passed).collect();
    let mut body = String::new();
    body.push_str(
        "Pre-approval CI-grade verification gate failed — this submission would \
         fail the PR Quality Gate, so it was returned for a fix instead of \
         opening a PR (no reopen/strike counted).\n\n",
    );
    body.push_str("Failing checks (run the same commands locally, fix, and resubmit):\n");
    for o in &failed {
        let command = SERVER_CHECK_SET
            .iter()
            .find(|c| c.name == o.name)
            .map(|c| c.command)
            .unwrap_or("");
        body.push_str(&format!(
            "\n- **{}** (exit {}): `{}`\n",
            o.name, o.exit_code, command
        ));
        let first = o
            .output
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        if !first.is_empty() {
            body.push_str(&format!("  first failure: {first}\n"));
        }
    }
    body
}

// ─── Verdict / enforcement ────────────────────────────────────────────────────

/// The outcome of running the gate for an approved task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreApprovalGateOutcome {
    /// Master kill-switch is off — gate did nothing.
    Disabled,
    /// No usable diff fingerprint (NoDiff / compute error) — cannot gate; proceed.
    SkippedNoFingerprint,
    /// Touched paths are docs-only / non-server — gate does not apply; proceed.
    BypassedNonServer,
    /// A cached green verify run for `(task, fingerprint)` covered the check set.
    PassCached,
    /// The check set was run inline and all checks passed.
    PassRan,
    /// Inline execution disabled and no cached verdict — deferred to the
    /// verification pod; proceed (record-only / shadow).
    DeferredNoVerdict,
    /// The check set failed. The task has been transitioned back to a worker
    /// round (strike-free) and the feedback was delivered.
    Blocked { feedback: String },
    /// A gate infra error occurred (fail-open — proceed).
    Error { reason: String },
}

impl PreApprovalGateOutcome {
    /// Whether the caller must block the PR-open / push.
    pub(crate) fn should_block(&self) -> bool {
        matches!(self, PreApprovalGateOutcome::Blocked { .. })
    }
}

/// Resolve the latest task-run id + workspace path for a task (the run that
/// produced the submission being gated), preferring a run with a workspace.
async fn resolve_task_run_workdir(
    db: &Database,
    task_id: &str,
) -> Option<(String, std::path::PathBuf)> {
    let repo = TaskRunRepository::new(db.clone());
    let runs = repo.list_for_task(task_id).await.ok()?;
    let (run_id, workspace) = runs
        .iter()
        .find(|r| r.workspace_path.as_deref().is_some_and(|p| !p.is_empty()))
        .and_then(|r| Some((r.id.clone(), r.workspace_path.clone()?)))?;
    let path = std::path::PathBuf::from(&workspace);
    if !path.exists() {
        return None;
    }
    Some((run_id, path))
}

/// Run the pre-approval CI-grade verification gate for an approved task.
///
/// On a red result this performs the strike-free re-route itself (feedback
/// activity + `approved → open` transition) and returns
/// [`PreApprovalGateOutcome::Blocked`]. All other outcomes mean "proceed with
/// the PR-open path".
///
/// `enabled` / `inline_exec` are injected so tests can drive the full path
/// without touching process env; production passes [`gate_enabled`] /
/// [`inline_exec_enabled`].
pub(crate) async fn evaluate_and_enforce(
    db: &Database,
    task_repo: &TaskRepository,
    task: &Task,
    enabled: bool,
    inline_exec: bool,
    runner: &dyn CiReproductionRunner,
) -> PreApprovalGateOutcome {
    if !enabled {
        return PreApprovalGateOutcome::Disabled;
    }

    // Only gate the approved → PR-push seam. Any other status is a caller bug
    // (fail-open rather than mis-transition).
    if TaskStatus::parse(&task.status).ok() != Some(TaskStatus::Approved) {
        return PreApprovalGateOutcome::Error {
            reason: format!(
                "preapproval gate invoked for task {} in status {} (expected approved)",
                task.short_id, task.status
            ),
        };
    }

    let Some((task_run_id, workdir)) = resolve_task_run_workdir(db, &task.id).await else {
        // No workspace to fingerprint or run checks against — cannot gate.
        return PreApprovalGateOutcome::SkippedNoFingerprint;
    };

    let fingerprint = match djinn_git::compute_submission_diff_fingerprint(&workdir).await {
        Ok(fp) => fp,
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "preapproval gate: failed to compute submission diff fingerprint; skipping"
            );
            return PreApprovalGateOutcome::SkippedNoFingerprint;
        }
    };

    let digest = match fingerprint.fingerprint() {
        Some(d) => d.to_string(),
        None => return PreApprovalGateOutcome::SkippedNoFingerprint,
    };

    // Docs-only / non-server bypass (mirrors the CI `changes` filter).
    if !changed_paths_trigger_server_gate(fingerprint.changed_paths()) {
        return PreApprovalGateOutcome::BypassedNonServer;
    }

    let required = required_check_names();
    let verify_repo = VerifyRunRepository::new(db.clone());

    // (task, fingerprint) cache: a prior green run covering the check set wins.
    match verify_repo
        .latest_pass_for_task_and_fingerprint(&task.id, &digest)
        .await
    {
        Ok(Some(run)) if coverage_covers_required(run.check_coverage.as_ref(), &required) => {
            tracing::info!(
                task_id = %task.short_id,
                fingerprint = %digest,
                "preapproval gate: cache hit (green verify run covers check set)"
            );
            return PreApprovalGateOutcome::PassCached;
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "preapproval gate: cache lookup failed; continuing"
            );
        }
    }

    // Select the checks to run. When inline execution is enabled we run the
    // full set. In shadow mode (inline exec off) we still run the no-compile
    // deterministic guards inline — they are exactly the deterministic
    // merge-queue offenders (Size Guard, Migrations Guard, Raw-SQL Boundary,
    // capability boundaries) and cost milliseconds — but defer the compile/DB
    // heavy checks (clippy, sqlx cache, test build) to the verification pod.
    let checks_to_run: Vec<PreApprovalCheck> = if inline_exec {
        SERVER_CHECK_SET.to_vec()
    } else {
        // The no-compile guards diff against origin/main; if the worktree has no
        // resolvable base they cannot run without being misread as violations,
        // so fall back to the historical deferral instead of false-blocking.
        if !base_ref_resolvable(runner, &workdir).await {
            tracing::debug!(
                task_id = %task.short_id,
                fingerprint = %digest,
                "preapproval gate: no cached verdict, inline exec off, and no resolvable diff base; deferring to verification pod"
            );
            return PreApprovalGateOutcome::DeferredNoVerdict;
        }
        no_build_checks()
    };

    // Run the selected checks and persist the verdict.
    let outcomes = run_checks(runner, &workdir, &checks_to_run).await;
    let all_pass = outcomes.iter().all(|o| o.passed);

    // Shadow mode + green no-compile subset: the deterministic guards passed but
    // the compile/DB checks were NOT run, so we must not persist a green run
    // (that would falsely satisfy the (task, fingerprint) cache and let a
    // build-failing resubmission through). Defer the remaining checks to the
    // verification pod and proceed, exactly as before.
    if all_pass && !inline_exec {
        tracing::debug!(
            task_id = %task.short_id,
            fingerprint = %digest,
            "preapproval gate: no-compile guards passed in shadow mode; deferring compile/DB checks to verification pod"
        );
        return PreApprovalGateOutcome::DeferredNoVerdict;
    }

    let coverage = check_coverage_json(&outcomes);
    let completed_at = now_rfc3339();
    let result = if all_pass {
        VerifyResult::Pass
    } else {
        VerifyResult::Fail
    };

    let verify_run_id = uuid::Uuid::now_v7().to_string();
    if let Err(e) = verify_repo
        .create(CreateVerifyRunParams {
            id: &verify_run_id,
            task_run_id: &task_run_id,
            verify_source: VerifySource::Local.as_str(),
            verify_run_id: "preapproval-gate",
            command_version: None,
            profile_version: Some("uv3p-server-v1"),
            completed_at: &completed_at,
            result: result.as_str(),
            diff_fingerprint: &digest,
            check_coverage: Some(&coverage),
        })
        .await
    {
        tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "preapproval gate: failed to persist verify_run row (non-fatal)"
        );
    }

    if all_pass {
        return PreApprovalGateOutcome::PassRan;
    }

    // ── Red result: strike-free feedback + return to worker round ────────────
    let feedback = format_gate_feedback(&outcomes);

    // Reviewer-style feedback: a `verification`-role `comment` is surfaced by the
    // worker prompt's `recent_feedback` channel under the "Verification failure"
    // label. Writing an activity row touches no reopen/strike counter.
    if let Err(e) = task_repo
        .log_activity(
            Some(&task.id),
            "coordinator",
            "verification",
            "comment",
            &serde_json::json!({ "body": feedback }).to_string(),
        )
        .await
    {
        tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "preapproval gate: failed to log verification feedback (non-fatal)"
        );
    }

    // Strike-free re-route: approved → open. `preapproval_verify_rejected`
    // increments no reopen/strike/intervention counter by construction.
    let reason =
        "pre-approval CI-grade verification gate failed; returned to worker round (strike-free)";
    if let Err(e) = task_repo
        .transition(
            &task.id,
            TransitionAction::PreApprovalVerifyRejected,
            "coordinator",
            "verification",
            Some(reason),
            None,
        )
        .await
    {
        tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "preapproval gate: failed to transition approved → open after red verdict"
        );
        return PreApprovalGateOutcome::Error {
            reason: format!("gate red but transition failed: {e}"),
        };
    }

    PreApprovalGateOutcome::Blocked { feedback }
}

/// Production entry point used by `supervisor_pr_open`. Reads the env switches
/// and uses the real [`ShellRunner`]. Returns the gate outcome; the caller
/// blocks the PR-open only when [`PreApprovalGateOutcome::should_block`].
pub(crate) async fn run_preapproval_gate(
    db: &Database,
    task_repo: &TaskRepository,
    task: &Task,
) -> PreApprovalGateOutcome {
    evaluate_and_enforce(
        db,
        task_repo,
        task,
        gate_enabled(),
        inline_exec_enabled(),
        &ShellRunner,
    )
    .await
}

/// Run the pre-approval CI-grade verification gate for an arbiter
/// approve/approve_conflict decision.
///
/// Semantically identical to [`run_preapproval_gate`] (same env switches,
/// same check set, same caching) but with two critical differences:
///
/// 1. **No task-status check.**  The reviewer gate expects `approved`;
///    the arbiter gate is called while the task is still
///    `in_lead_intervention`.  The workspace and fingerprint are the
///    same — only the status seam differs.
///
/// 2. **No board transition on red.**  The reviewer gate fires
///    `PreApprovalVerifyRejected` (`approved → open`) on a red result.
///    The arbiter gate must NOT transition the task — it returns
///    [`ArbiterGateResult::Blocked`] and the caller is responsible for
///    leaving the task in `in_lead_intervention` (the arbitration row
///    stays unconsumed, no strike, no decision-failure increment).
///
/// Activity feedback is still logged on red so the arbiter session's
/// `recent_feedback` channel carries the check failures.
pub async fn run_arbiter_preapproval_gate(
    db: &Database,
    task_repo: &TaskRepository,
    task: &Task,
) -> djinn_supervisor::ArbiterGateResult {
    evaluate_arbiter_gate(
        db,
        task_repo,
        task,
        gate_enabled(),
        inline_exec_enabled(),
        &ShellRunner,
    )
    .await
}

/// Core arbiter-gate evaluation.  Mirrors [`evaluate_and_enforce`] but skips
/// the task-status check and the `PreApprovalVerifyRejected` transition on
/// red.
async fn evaluate_arbiter_gate(
    db: &Database,
    task_repo: &TaskRepository,
    task: &Task,
    enabled: bool,
    inline_exec: bool,
    runner: &dyn CiReproductionRunner,
) -> djinn_supervisor::ArbiterGateResult {
    if !enabled {
        return djinn_supervisor::ArbiterGateResult::Pass;
    }

    let Some((_task_run_id, workdir)) = resolve_task_run_workdir(db, &task.id).await else {
        return djinn_supervisor::ArbiterGateResult::Pass;
    };

    let fingerprint = match djinn_git::compute_submission_diff_fingerprint(&workdir).await {
        Ok(fp) => fp,
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "arbiter preapproval gate: failed to compute submission diff fingerprint; skipping"
            );
            return djinn_supervisor::ArbiterGateResult::Pass;
        }
    };

    let digest = match fingerprint.fingerprint() {
        Some(d) => d.to_string(),
        None => return djinn_supervisor::ArbiterGateResult::Pass,
    };

    if !changed_paths_trigger_server_gate(fingerprint.changed_paths()) {
        return djinn_supervisor::ArbiterGateResult::Pass;
    }

    let required = required_check_names();
    let verify_repo = VerifyRunRepository::new(db.clone());

    match verify_repo
        .latest_pass_for_task_and_fingerprint(&task.id, &digest)
        .await
    {
        Ok(Some(run)) if coverage_covers_required(run.check_coverage.as_ref(), &required) => {
            tracing::info!(
                task_id = %task.short_id,
                fingerprint = %digest,
                "arbiter preapproval gate: cache hit (green verify run covers check set)"
            );
            return djinn_supervisor::ArbiterGateResult::Pass;
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "arbiter preapproval gate: cache lookup failed; continuing"
            );
        }
    }

    // Same split as the reviewer gate: the no-compile deterministic guards run
    // inline even in shadow mode; the compile/DB checks defer to the
    // verification pod unless inline execution is enabled.
    let checks_to_run: Vec<PreApprovalCheck> = if inline_exec {
        SERVER_CHECK_SET.to_vec()
    } else {
        if !base_ref_resolvable(runner, &workdir).await {
            tracing::debug!(
                task_id = %task.short_id,
                fingerprint = %digest,
                "arbiter preapproval gate: no cached verdict, inline exec off, and no resolvable diff base; deferring to verification pod"
            );
            return djinn_supervisor::ArbiterGateResult::Pass;
        }
        no_build_checks()
    };

    let outcomes = run_checks(runner, &workdir, &checks_to_run).await;
    let all_pass = outcomes.iter().all(|o| o.passed);

    // Shadow mode + green no-compile subset: defer the compile/DB checks (do not
    // persist a partial-green run that would falsely satisfy the cache).
    if all_pass && !inline_exec {
        tracing::debug!(
            task_id = %task.short_id,
            fingerprint = %digest,
            "arbiter preapproval gate: no-compile guards passed in shadow mode; deferring compile/DB checks to verification pod"
        );
        return djinn_supervisor::ArbiterGateResult::Pass;
    }

    let coverage = check_coverage_json(&outcomes);
    let completed_at = now_rfc3339();
    let result = if all_pass {
        VerifyResult::Pass
    } else {
        VerifyResult::Fail
    };

    let verify_run_id = uuid::Uuid::now_v7().to_string();
    if let Err(e) = verify_repo
        .create(CreateVerifyRunParams {
            id: &verify_run_id,
            task_run_id: "", // no task_run context for arbiter gate
            verify_source: VerifySource::Local.as_str(),
            verify_run_id: "arbiter-preapproval-gate",
            command_version: None,
            profile_version: Some("uv3p-server-v1"),
            completed_at: &completed_at,
            result: result.as_str(),
            diff_fingerprint: &digest,
            check_coverage: Some(&coverage),
        })
        .await
    {
        tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "arbiter preapproval gate: failed to persist verify_run row (non-fatal)"
        );
    }

    if all_pass {
        return djinn_supervisor::ArbiterGateResult::Pass;
    }

    // ── Red result: log feedback, return Blocked (no transition) ─────────
    let feedback = format_gate_feedback(&outcomes);

    if let Err(e) = task_repo
        .log_activity(
            Some(&task.id),
            "coordinator",
            "verification",
            "comment",
            &serde_json::json!({ "body": feedback }).to_string(),
        )
        .await
    {
        tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "arbiter preapproval gate: failed to log verification feedback (non-fatal)"
        );
    }

    djinn_supervisor::ArbiterGateResult::Blocked { feedback }
}

// ─── Utility ──────────────────────────────────────────────────────────────────

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn tail_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let start = s.len() - max_bytes;
    match s[start..].char_indices().next() {
        Some((i, _)) => s[start + i..].to_owned(),
        None => String::new(),
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
        let epic_repo =
            djinn_db::repositories::epic::EpicRepository::new(db.clone(), EventBus::noop());
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
}
