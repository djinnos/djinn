//! Deploy-time preflight: would this RENDERED chart be able to dispatch?
//!
//! On 2026-07-29 a release shipped a fail-closed dispatch validator
//! ([`validate_enforcement_render`]) whose rejection condition was *the chart's
//! own default values*: `cgroupLauncher.mode: required` paired with
//! `cgroupWritable.taskRuns.enabled: false`. Every dispatch died at
//! `runtime.prepare` with `MissingDelegatedRuntimeClass` before a Job was ever
//! submitted, and no repository test noticed, because nothing ran the real
//! validator against a real render.
//!
//! This binary is that missing link, and it is deliberately thin: it rebuilds
//! the `KubernetesConfig` djinn-server itself builds at startup — via the same
//! [`KubernetesConfig::from_env`] the server calls, so env-name drift or a
//! parsing quirk is inherited rather than re-implemented — and hands it to the
//! REAL `validate_enforcement_render`. There is no second copy of the rule
//! here; if the crate's rule changes, this gate changes with it.
//!
//! The environment it reads is the environment of *the rendered djinn-server
//! container*, not of whoever ran the deploy: `deploy/preflight/render-gate.sh`
//! extracts it from `helm template` output and re-execs this binary under
//! `env -i`, so the only `DJINN_K8S_*` values visible here are the ones the
//! cluster would give the server pod.
//!
//! Exit status is the whole contract: `0` — the render dispatches; `1` — it
//! does not, and stderr names the rejecting `RenderValidationError` variant.

use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::launcher::validate_enforcement_render;
use std::process::ExitCode;

#[allow(clippy::print_stdout, clippy::print_stderr)]
fn main() -> ExitCode {
    let config = KubernetesConfig::from_env();
    // Echoed on both paths: a gate that says only "REJECT" leaves an operator
    // guessing which of the rendered knobs produced the verdict.
    let summary = format!(
        "cgroup_launcher_mode={} task_run_cgroup_writable_enabled={} \
         cgroup_delegation_profile={} volume_ownership_mode={} cpu_limit={}",
        config.cgroup_launcher_mode.as_str(),
        config.task_run_cgroup_writable_enabled,
        config.cgroup_delegation_profile,
        config.volume_ownership_mode,
        config.cpu_limit,
    );

    match validate_enforcement_render(&config) {
        Ok(()) => {
            println!("render-gate: DISPATCHABLE {summary}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            // `{error:?}` names the variant (e.g. `MissingDelegatedRuntimeClass`)
            // and `{error}` carries the operator-facing explanation. Callers —
            // including deploy/preflight/tests/render-gate.sh — assert on the
            // variant name, which is stable in a way prose is not.
            eprintln!("render-gate: UNDISPATCHABLE {summary}");
            eprintln!("render-gate: RenderValidationError::{error:?}");
            eprintln!("render-gate: {error}");
            ExitCode::FAILURE
        }
    }
}
