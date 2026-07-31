//! Deploy-time driver for the `3i92` CPU-quota cutover preflight.
//!
//! Mirrors `bin/render-gate.rs` deliberately: a thin shell whose only job is to
//! assemble real observations and hand them to the REAL validator
//! ([`djinn_k8s::cutover_preflight::run`]). There is no second copy of any rule
//! here. If the crate's rule changes, this driver changes with it.
//!
//! # What it assembles
//!
//! * **The Helm surface** — the `pods/resize` Role rule, the task-run
//!   ServiceAccount and every RoleBinding — comes from a LIVE `helm template`
//!   render, converted to JSON and passed as `argv[1]`.
//!   `deploy/preflight/cutover-preflight.sh` produces it.
//! * **The Rust surface** — `automountServiceAccountToken`, the projected token
//!   audience and the launcher sidecar's CPU ceiling — is rendered here, in
//!   process, by the same [`djinn_k8s::job::build_task_run_job`] +
//!   [`djinn_k8s::launcher::apply_launcher_authority_protocol`] pair dispatch
//!   uses. Helm never sees those fields, so a Helm-only preflight would declare
//!   a credential boundary it had not looked at.
//! * **The durable drain fence** comes from Postgres via the production
//!   `list_nonterminal_resize` query when `DJINN_DATABASE_URL` is set. When it
//!   is NOT set the fence is reported UNOBSERVABLE, which is a defect: "the
//!   database was unreachable" and "nothing is in flight" are the two answers a
//!   cutover must never confuse.
//! * **The signed legacy-digest inventory** is resolved by
//!   [`LegacyDigestInventory::from_env`] — the production resolver, with its
//!   own fail-closed `Unusable` arm. It is deliberately NOT taken from the
//!   observation bundle: a file that could declare itself verified is not an
//!   inventory.
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
use std::str::FromStr;

use djinn_cgroup_launcher::LauncherAuthorityProtocol;
use djinn_db::launcher_compatibility::LegacyDigestInventory;
use djinn_db::{BuildPodPermitRepository, Database, DatabaseConnectConfig, PostgresDatabaseConfig};
use djinn_k8s::config::KubernetesConfig;
use djinn_k8s::cutover_preflight::{
    BirthObservation, CatalogImage, CutoverPreflightInput, DrainFenceObservation,
    observe_drain_fence, run, summarize,
};
use djinn_k8s::job::build_task_run_job;
use djinn_k8s::launcher::apply_launcher_authority_protocol;
use k8s_openapi::api::core::v1::Pod;
use serde::Deserialize;
use serde_json::Value;

/// Cluster/registry observations a render cannot contain.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observations {
    #[serde(default)]
    catalog: Vec<WireCatalogImage>,
    #[serde(default)]
    births: Vec<WireBirth>,
    #[serde(default)]
    live_task_run_pods: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCatalogImage {
    pull_ref: String,
    /// `"leaf-v1"`, `"resize-v2"`, or absent for a pre-handshake image.
    #[serde(default)]
    declared: Option<String>,
    #[serde(default)]
    digest: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBirth {
    pod: Pod,
    target_cpu: String,
}

/// Placeholders for the task-run Job render.
///
/// None of the three participate in any property this preflight judges: the
/// credential boundary and the launcher ceiling are functions of the config and
/// the authority protocol, not of which task run happens to be dispatching.
/// Naming them here rather than reading them from somewhere makes the render
/// reproducible, which is what lets the wrapper's fixture diff be exact.
const PREFLIGHT_PROJECT: &str = "cutover-preflight";
const PREFLIGHT_SECRET: &str = "cutover-preflight-secret";
const PREFLIGHT_IMAGE: &str = "ghcr.io/djinnos/cutover-preflight:preflight";

#[allow(clippy::print_stdout, clippy::print_stderr)]
fn main() -> ExitCode {
    match drive() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("cutover-preflight: UNEVALUABLE {message}");
            ExitCode::from(2)
        }
    }
}

#[allow(clippy::print_stdout, clippy::print_stderr)]
fn drive() -> Result<bool, String> {
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

    let manifests = load_manifests(&path)?;
    let mode = authority_mode()?;
    let observations = load_observations()?;

    let config = KubernetesConfig::from_env();
    // `build_task_run_job` asserts this pairing. It is `render-gate`'s subject,
    // not this one's, so refuse to evaluate rather than panic in a deploy step.
    if config.cgroup_launcher_mode.renders_sidecar() && !config.task_run_cgroup_writable_enabled {
        return Err(
            "the rendered config arms the cgroup launcher without the task-run RuntimeClass; \
             deploy/preflight/render-gate.sh is the gate for that pairing and refuses it already"
                .to_string(),
        );
    }
    let mut job = build_task_run_job(
        &config,
        &uuid::Uuid::nil(),
        PREFLIGHT_PROJECT,
        PREFLIGHT_SECRET,
        PREFLIGHT_IMAGE,
        &[],
        None,
        false,
        None,
    );
    // The same post-render seam dispatch uses. Its refusals ARE ceiling
    // defects, so they are folded into the report instead of aborting: an
    // operator needs every blocking reason in one run, not the first one.
    let applied = apply_launcher_authority_protocol(&mut job, config.cgroup_launcher_mode, mode);

    // The production resolver, not the bundle: an inventory that could be
    // handed to the gate by the thing being gated is not an inventory.
    let inventory = LegacyDigestInventory::from_env();
    let catalog = observations
        .catalog
        .into_iter()
        .map(|image| {
            let declared = match image.declared.as_deref() {
                None => None,
                Some(raw) => Some(
                    LauncherAuthorityProtocol::from_str(raw)
                        .map_err(|error| format!("catalog image {:?}: {error}", image.pull_ref))?,
                ),
            };
            Ok(CatalogImage {
                pull_ref: image.pull_ref,
                declared,
                digest: image.digest,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let births: Vec<BirthObservation> = observations
        .births
        .into_iter()
        .map(|birth| BirthObservation {
            pod: birth.pod,
            target_cpu: birth.target_cpu,
        })
        .collect();
    let drain = drain_fence(observations.live_task_run_pods);

    let input = CutoverPreflightInput {
        manifests: &manifests,
        task_run_job: &job,
        authority_mode: mode,
        catalog: &catalog,
        legacy_digest_inventory: &inventory,
        births: &births,
        drain: &drain,
    };
    let summary = summarize(&input);

    let mut blocked = false;
    if let Err(error) = applied {
        // Emitted with the same class prefix the validator uses, so the shell
        // contract asserts one vocabulary rather than two.
        eprintln!(
            "cutover-preflight: DEFECT launcher-cpu-ceiling the dispatch render refused to apply \
             the {mode} authority protocol: RenderValidationError::{error:?}: {error}"
        );
        blocked = true;
    }
    match run(&input) {
        Ok(report) if !blocked => {
            println!(
                "cutover-preflight: CLEAR {summary} evaluated={}",
                report
                    .evaluated()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            );
            Ok(true)
        }
        Ok(_) => {
            eprintln!("cutover-preflight: BLOCKED {summary} classes=launcher-cpu-ceiling");
            Ok(false)
        }
        Err(refusal) => {
            eprintln!("cutover-preflight: BLOCKED {summary}");
            for defect in refusal.defects() {
                eprintln!(
                    "cutover-preflight: DEFECT {} {}",
                    defect.class(),
                    defect.detail()
                );
            }
            let mut classes: Vec<String> =
                refusal.classes().iter().map(ToString::to_string).collect();
            if blocked {
                classes.push("launcher-cpu-ceiling".to_string());
                classes.sort();
                classes.dedup();
            }
            eprintln!(
                "cutover-preflight: BLOCKED classes={} defects={}",
                classes.join(","),
                refusal.defects().len()
            );
            Ok(false)
        }
    }
}

/// The render, as a flat list of documents.
///
/// JSON rather than YAML because `serde_yaml` is a dev-dependency of this
/// crate; the wrapper converts with the same `python3`/PyYAML pair
/// `render-gate.sh` already requires. A single document, a JSON array, and a
/// `{"items": [...]}` list are all accepted, because all three are shapes
/// `helm template` output legitimately reduces to.
fn load_manifests(path: &str) -> Result<Vec<Value>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read rendered manifests {path}: {error}"))?;
    let parsed: Value = serde_json::from_str(&raw)
        .map_err(|error| format!("rendered manifests {path} are not valid JSON: {error}"))?;
    let documents = match parsed {
        Value::Array(documents) => documents,
        Value::Object(ref map) if map.contains_key("items") => map["items"]
            .as_array()
            .cloned()
            .ok_or_else(|| format!("{path}: `items` is not an array"))?,
        other => vec![other],
    };
    if documents.is_empty() {
        return Err(format!(
            "{path} contains no documents; an empty render would pass every render-derived check \
             vacuously"
        ));
    }
    Ok(documents)
}

/// The protocol the deployment is being flipped to. Fail-closed on a malformed
/// value; absent means the pre-cutover status quo, which is `leaf-v1`.
fn authority_mode() -> Result<LauncherAuthorityProtocol, String> {
    match std::env::var("DJINN_CUTOVER_AUTHORITY_MODE") {
        Err(_) => Ok(LauncherAuthorityProtocol::LeafV1),
        Ok(raw) if raw.trim().is_empty() => Ok(LauncherAuthorityProtocol::LeafV1),
        Ok(raw) => LauncherAuthorityProtocol::from_str(raw.trim())
            .map_err(|error| format!("DJINN_CUTOVER_AUTHORITY_MODE: {error}")),
    }
}

fn load_observations() -> Result<Observations, String> {
    let Ok(path) = std::env::var("DJINN_CUTOVER_OBSERVATIONS") else {
        return Ok(Observations::default());
    };
    if path.trim().is_empty() {
        return Ok(Observations::default());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read observations {path}: {error}"))?;
    serde_json::from_str(&raw).map_err(|error| format!("observations {path} are invalid: {error}"))
}

/// Read the durable half of the drain fence with the production query, or
/// report it unobservable. Never reports an empty fence it did not read.
fn drain_fence(live_task_run_pods: Vec<String>) -> DrainFenceObservation {
    let Ok(url) = std::env::var("DJINN_DATABASE_URL") else {
        return DrainFenceObservation::unobservable(
            "DJINN_DATABASE_URL is not set, so the nonterminal resize/lease rows could not be read",
        );
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return DrainFenceObservation::unobservable(format!("no tokio runtime: {error}"));
        }
    };
    // The pool is built INSIDE `block_on`. `PgPool` spawns its own reaper task
    // at construction, so `sqlx` panics with "this functionality requires a
    // Tokio context" if it is created outside a runtime — and a panic in a
    // deploy gate is an exit 101 that reads as neither a clean nor a blocked
    // verdict. Caught by `deploy/preflight/tests/cutover-preflight.sh` the
    // first time the lane ran it with a real `DJINN_DATABASE_URL`.
    runtime.block_on(async move {
        let database = match Database::open_with_config(DatabaseConnectConfig::Postgres(
            PostgresDatabaseConfig { url },
        )) {
            Ok(database) => database,
            Err(error) => {
                return DrainFenceObservation::unobservable(format!(
                    "cannot open DJINN_DATABASE_URL: {error}"
                ));
            }
        };
        let permits = BuildPodPermitRepository::new(database);
        observe_drain_fence(&permits, live_task_run_pods).await
    })
}
