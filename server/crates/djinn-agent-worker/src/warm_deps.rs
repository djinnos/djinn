//! Best-effort JS dependency install on the warm path.
//!
//! `scip-typescript` can only index a workspace once `node_modules` exists.
//! A monorepo whose `tsconfig.json` does `extends: "tsconfig/base.json"` (a
//! workspace package), or that uses project references / `@types`, resolves
//! all of that through `node_modules`. Against an empty tree every target
//! fails with "missing tsconfig.json" → `run_indexers` reports "all N SCIP
//! indexers failed" → the warm Pod exits 1 and re-dispatches every ~60s
//! forever (observed for `alt-front-end`).
//!
//! We detect the JS package manager from the cloned repo (reusing
//! [`djinn_stack`] — the `packageManager` field first, lockfile presence as
//! fallback) and run `<pm> install` before the canonical-graph pipeline.
//!
//! Everything here is **best-effort**: any failure is logged and swallowed
//! so indexing still runs (degraded, exactly as before this step existed).
//! It is also a no-op for non-JS projects — a Rust/Python/Go warm detects no
//! JS package manager and returns immediately.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::process::Command;
use tracing::{info, warn};

/// Hard cap so a wedged install can't eat the whole warm-Job
/// `activeDeadlineSeconds` (1800s) and starve the indexers that follow.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);

/// JS package-manager slugs we know how to drive, in priority order. Matches
/// the canonical slugs emitted by [`djinn_stack`].
const JS_PACKAGE_MANAGERS: &[&str] = &["pnpm", "yarn", "bun", "npm"];

/// Detect the JS package manager at `project_root` and install dependencies
/// so the TypeScript indexer has a populated `node_modules`. Never returns an
/// error — the warm pipeline must proceed regardless of the outcome.
pub async fn maybe_install_node_deps(project_root: &Path) {
    let stack = match djinn_stack::detect::detect(project_root).await {
        Ok(stack) => stack,
        Err(e) => {
            warn!(
                error = %format!("{e:#}"),
                "warm deps: stack detection failed; skipping JS dependency install"
            );
            return;
        }
    };

    // JS/TS projects only: prefer an explicit package-manager slug (from the
    // `packageManager` field or lockfile), else fall back to npm when a Node
    // runtime is declared. No match → not a JS project, nothing to install.
    let pm = JS_PACKAGE_MANAGERS
        .iter()
        .find(|pm| stack.package_managers.iter().any(|slug| slug == *pm))
        .copied()
        .or_else(|| stack.runtimes.node.as_ref().map(|_| "npm"));
    let Some(pm) = pm else {
        info!("warm deps: no JS package manager detected; skipping install");
        return;
    };

    info!(
        package_manager = pm,
        project_root = %project_root.display(),
        "warm deps: installing JS dependencies so scip-typescript can resolve workspace configs"
    );

    let attempts = install_attempts(pm);
    for (idx, (program, args)) in attempts.iter().enumerate() {
        match run_install(project_root, program, args).await {
            Ok(true) => {
                info!(
                    package_manager = pm,
                    attempt = idx,
                    "warm deps: install succeeded"
                );
                return;
            }
            Ok(false) => warn!(
                package_manager = pm,
                attempt = idx,
                program,
                "warm deps: install attempt failed (non-zero exit / timeout)"
            ),
            Err(e) => warn!(
                package_manager = pm,
                attempt = idx,
                program,
                error = %format!("{e:#}"),
                "warm deps: install attempt errored"
            ),
        }
    }
    warn!(
        package_manager = pm,
        "warm deps: all install attempts failed; indexing with no node_modules (TS targets may fail)"
    );
}

/// `(program, args)` pairs to try in order.
///
/// `--ignore-scripts` skips pre/postinstall hooks (`only-allow pnpm`,
/// `husky install`, native rebuilds) — they need extra tooling/network and
/// are irrelevant to indexing.
///
/// The first attempt routes pnpm/yarn through **corepack**, which honors the
/// repo's `packageManager` pin (e.g. `pnpm@8.15.9`). That matters because the
/// image installs the *latest* pnpm globally, and a newer pnpm refuses a
/// `--frozen-lockfile` install against an older-format lockfile. The second
/// attempt drops `--frozen-lockfile` and uses the image's global PM, so a
/// drifted or older-format lockfile still installs (it just re-resolves).
fn install_attempts(pm: &str) -> Vec<(&'static str, Vec<&'static str>)> {
    match pm {
        "pnpm" => vec![
            (
                "corepack",
                vec!["pnpm", "install", "--frozen-lockfile", "--ignore-scripts"],
            ),
            ("pnpm", vec!["install", "--ignore-scripts"]),
        ],
        "yarn" => vec![
            (
                "corepack",
                vec!["yarn", "install", "--frozen-lockfile", "--ignore-scripts"],
            ),
            ("yarn", vec!["install", "--ignore-scripts"]),
        ],
        "bun" => vec![
            (
                "bun",
                vec!["install", "--frozen-lockfile", "--ignore-scripts"],
            ),
            ("bun", vec!["install", "--ignore-scripts"]),
        ],
        // npm: `ci` is the frozen form but it hard-requires a lockfile in sync
        // and wipes node_modules; `install` is more forgiving for a throwaway
        // warm workspace.
        _ => vec![(
            "npm",
            vec!["install", "--ignore-scripts", "--no-audit", "--no-fund"],
        )],
    }
}

/// Run one install command. Returns `Ok(true)` on success, `Ok(false)` on a
/// non-zero exit or timeout, `Err` only when the process couldn't be spawned
/// or waited on.
async fn run_install(project_root: &Path, program: &str, args: &[&str]) -> Result<bool> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(project_root)
        // Non-interactive: corepack must never block on a
        // "download pnpm@x? (Y/n)" prompt inside a headless Pod.
        .env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // On timeout the future is dropped; kill the child so it can't linger.
        .kill_on_drop(true);

    let child = cmd.spawn().with_context(|| format!("spawn `{program}`"))?;

    let output = match tokio::time::timeout(INSTALL_TIMEOUT, child.wait_with_output()).await {
        Ok(result) => result.with_context(|| format!("wait `{program}`"))?,
        Err(_) => {
            warn!(
                program,
                timeout_s = INSTALL_TIMEOUT.as_secs(),
                "warm deps: install timed out"
            );
            return Ok(false);
        }
    };

    if !output.status.success() {
        // Surface a trimmed stderr tail so `kubectl logs` explains the failure
        // without dumping a multi-MB install log.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let lines: Vec<&str> = stderr.lines().collect();
        let tail = lines[lines.len().saturating_sub(8)..].join("\n");
        warn!(
            program,
            exit = ?output.status.code(),
            stderr_tail = %tail,
            "warm deps: install non-zero exit"
        );
    }

    Ok(output.status.success())
}
