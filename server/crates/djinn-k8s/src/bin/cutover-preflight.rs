//! Deploy-time driver for the `3i92` CPU-quota cutover preflight.
//!
//! Mirrors `bin/render-gate.rs` deliberately: a thin shell whose only job is to
//! assemble real observations and hand them to the REAL validator
//! ([`djinn_k8s::cutover_preflight::run`]). There is no second copy of any rule
//! here. If the crate's rule changes, this driver changes with it.
//!
//! # What it assembles
//!
//! The assembly itself lives in [`djinn_k8s::cutover_preflight_driver`], not in
//! this file, because `djinn-server`'s authority-cutover driver runs the same
//! preflight before it flips the authority mode and must not assemble a
//! *different* one. What that module gathers:
//!
//! * **The Helm surface** — the `pods/resize` Role rule, the task-run
//!   ServiceAccount and every RoleBinding — comes from a LIVE `helm template`
//!   render, converted to JSON and passed as `argv[1]`.
//!   `deploy/preflight/cutover-preflight.sh` produces it.
//! * **The Rust surface** — `automountServiceAccountToken`, the projected token
//!   audience and the launcher sidecar's CPU ceiling — is rendered in process by
//!   the same [`djinn_k8s::job::build_task_run_job`] +
//!   [`djinn_k8s::launcher::apply_launcher_authority_protocol`] pair dispatch
//!   uses. Helm never sees those fields, so a Helm-only preflight would declare
//!   a credential boundary it had not looked at.
//! * **The durable drain fence** comes from Postgres via the production
//!   `list_nonterminal_resize` query when `DJINN_DATABASE_URL` is set. When it
//!   is NOT set the fence is reported UNOBSERVABLE, which is a defect: "the
//!   database was unreachable" and "nothing is in flight" are the two answers a
//!   cutover must never confuse.
//! * **The signed legacy-digest inventory** is resolved by
//!   [`djinn_db::launcher_compatibility::LegacyDigestInventory::from_env`] — the
//!   production resolver, with its own fail-closed `Unusable` arm. It is
//!   deliberately NOT taken from the observation bundle: a file that could
//!   declare itself verified is not an inventory.
//! * **Catalog images and live birth observations** come from the JSON bundle
//!   at `DJINN_CUTOVER_OBSERVATIONS`. These are cluster/registry facts a render
//!   cannot contain. An absent bundle means an empty catalog and no births —
//!   which is *vacuously* clean and therefore never used as the sole evidence
//!   for those classes; the integration suite carries the real fixtures.
//!
//! # Environment
//!
//! The `DJINN_K8S_*` variables are read from the environment this process is
//! given, exactly as `render-gate` reads them: the wrapper script extracts them
//! from the RENDERED djinn-server container and re-execs under `env -i`, so the
//! verdict depends on the render and never on the operator's shell.
//!
//! `DJINN_CUTOVER_AUTHORITY_MODE` names the protocol the deployment is being
//! flipped to (`leaf-v1` or `resize-v2`). A malformed value is refused rather
//! than defaulted: defaulting it would answer a question about `resize-v2`
//! readiness with a `leaf-v1` verdict.
//!
//! # Exit status
//!
//! * `0` — clean; the cutover may proceed.
//! * `1` — at least one defect; every one is printed with its class.
//! * `2` — the preflight could not be evaluated at all (bad arguments,
//!   unparseable render, a config `render-gate` already refuses). Distinct from
//!   `1` so a harness error is never read as a clean or a blocked verdict.

use std::process::ExitCode;

use djinn_k8s::cutover_preflight_driver::{
    DrainFenceSource, PreflightSources, RenderedCutoverPreflight, authority_mode_from_env,
};

#[allow(clippy::print_stdout, clippy::print_stderr)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match drive().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("cutover-preflight: UNEVALUABLE {message}");
            ExitCode::from(2)
        }
    }
}

#[allow(clippy::print_stdout, clippy::print_stderr)]
async fn drive() -> Result<bool, String> {
    let path = std::env::args().nth(1).ok_or_else(|| {
        "usage: cutover-preflight <rendered-manifests.json>  (the wrapper \
         deploy/preflight/cutover-preflight.sh produces the file)"
            .to_string()
    })?;
    if path == "-h" || path == "--help" {
        println!(
            "usage: cutover-preflight <rendered-manifests.json>\n\
             env: DJINN_CUTOVER_AUTHORITY_MODE={{leaf-v1|resize-v2}} \
             DJINN_CUTOVER_OBSERVATIONS=<bundle.json> DJINN_DATABASE_URL=<postgres url>\n\
             exit: 0 clear, 1 blocked, 2 unevaluable"
        );
        return Ok(true);
    }

    let mode = authority_mode_from_env()?;
    let preflight = RenderedCutoverPreflight::load(
        &PreflightSources::from_env(path),
        DrainFenceSource::from_database_url_env(),
    )?;
    // This binary reads no apiserver of its own; the Pod half of the fence comes
    // from the observation bundle. `djinn-server`'s cutover driver enumerates it
    // live and passes it here instead.
    let judgement = preflight.judge(mode, &[]).await?;
    let summary = &judgement.summary;

    if judgement.is_clear() {
        println!(
            "cutover-preflight: CLEAR {summary} evaluated={}",
            judgement.evaluated_classes().join(",")
        );
        return Ok(true);
    }

    eprintln!("cutover-preflight: BLOCKED {summary}");
    for defect in judgement.blocking_defects() {
        eprintln!("cutover-preflight: DEFECT {defect}");
    }
    eprintln!(
        "cutover-preflight: BLOCKED classes={} defects={}",
        judgement.blocking_classes().join(","),
        judgement.blocking_defects().len()
    );
    Ok(false)
}
