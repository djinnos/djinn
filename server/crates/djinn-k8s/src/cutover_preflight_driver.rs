//! Assembling a REAL [`crate::cutover_preflight`] input, once, for every caller
//! that needs a verdict (proposal `3i92`).
//!
//! # Why this is a library module and not two copies
//!
//! [`crate::cutover_preflight::run`] judges an input it does not assemble. The
//! assembly — a live `helm template` render, the Rust-rendered task-run Job, the
//! signed inventory resolved from the deployment's environment, and the durable
//! drain fence read with the production query — lives here so the permanent
//! deploy gate has one testable assembly boundary.
//!
//! The retired one-shot authority-cutover driver also consumed this boundary.
//! The permanent deploy gate continues to use [`RenderedCutoverPreflight`].
//!
//! # What a caller must supply, and what it must not
//!
//! It supplies the *render* (as JSON documents) and the cluster/registry
//! observations a render cannot contain. It does **not** supply the signed
//! legacy-digest inventory or the drain fence: the inventory comes from
//! [`LegacyDigestInventory::from_env`], the production resolver, and the fence
//! comes from [`observe_drain_fence`] over the production
//! `list_nonterminal_resize` query. A gate whose evidence can be handed to it by
//! the thing being gated is not a gate.

use std::str::FromStr;

use djinn_cgroup_launcher::LauncherAuthorityProtocol;
use djinn_db::launcher_compatibility::LegacyDigestInventory;
use djinn_db::{BuildPodPermitRepository, Database, DatabaseConnectConfig, PostgresDatabaseConfig};
use k8s_openapi::api::core::v1::Pod;
use serde::Deserialize;
use serde_json::Value;

use crate::config::KubernetesConfig;
use crate::cutover_preflight::{
    BirthObservation, Blocked, CatalogImage, CutoverPreflightInput, DefectClass,
    DrainFenceObservation, observe_drain_fence, run, summarize,
};
use crate::job::build_task_run_job;
use crate::launcher::apply_launcher_authority_protocol;

/// Cluster/registry observations a render cannot contain.
///
/// Deserialized from the `DJINN_CUTOVER_OBSERVATIONS` bundle. `deny_unknown_fields`
/// is deliberate: a typo'd key that silently produced an empty catalog would
/// make the catalog class pass vacuously.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observations {
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

/// Where the caller's half of the evidence lives on disk.
#[derive(Clone, Debug)]
pub struct PreflightSources {
    /// A LIVE `helm template` render, converted to JSON. A single document, a
    /// JSON array and a `{"items": [...]}` list are all accepted.
    pub manifests_path: String,
    /// The `DJINN_CUTOVER_OBSERVATIONS` bundle, when one is configured.
    pub observations_path: Option<String>,
}

impl PreflightSources {
    /// The sources a deploy step is given: the render as `argv[1]`, the bundle
    /// from `DJINN_CUTOVER_OBSERVATIONS`.
    #[must_use]
    pub fn from_env(manifests_path: impl Into<String>) -> Self {
        let observations_path = std::env::var("DJINN_CUTOVER_OBSERVATIONS")
            .ok()
            .filter(|path| !path.trim().is_empty());
        Self {
            manifests_path: manifests_path.into(),
            observations_path,
        }
    }
}

/// Where the durable half of the drain fence comes from.
///
/// Two variants and no third, because "the database was unreachable" and
/// "nothing is in flight" are the two answers a cutover must never confuse. An
/// [`Self::Unobservable`] fence is a *defect*, not an empty one.
pub enum DrainFenceSource {
    /// The production `list_nonterminal_resize` query, against a live pool.
    Repository(BuildPodPermitRepository),
    /// The fence could not be read. Fails closed.
    Unobservable(String),
}

impl DrainFenceSource {
    /// Open `DJINN_DATABASE_URL`, or report the fence unobservable.
    ///
    /// # Panics
    ///
    /// Must be called from inside a Tokio runtime: `PgPool` spawns its own
    /// reaper task at construction, and `sqlx` panics with "this functionality
    /// requires a Tokio context" otherwise — a panic in a deploy gate is an
    /// exit 101 that reads as neither a clean nor a blocked verdict. Caught by
    /// `deploy/preflight/tests/cutover-preflight.sh` the first time the lane ran
    /// it with a real `DJINN_DATABASE_URL`.
    #[must_use]
    pub fn from_database_url_env() -> Self {
        let Ok(url) = std::env::var("DJINN_DATABASE_URL") else {
            return Self::Unobservable(
                "DJINN_DATABASE_URL is not set, so the nonterminal resize/lease rows could not be \
                 read"
                    .to_string(),
            );
        };
        match Database::open_with_config(DatabaseConnectConfig::Postgres(PostgresDatabaseConfig {
            url,
        })) {
            Ok(database) => Self::Repository(BuildPodPermitRepository::new(database)),
            Err(error) => Self::Unobservable(format!("cannot open DJINN_DATABASE_URL: {error}")),
        }
    }
}

/// A loaded, judgeable preflight.
///
/// Loading is separated from judging so a caller that must *not* proceed on an
/// unreadable render finds out before it has paused admission or moved anything.
pub struct RenderedCutoverPreflight {
    manifests: Vec<Value>,
    catalog: Vec<CatalogImage>,
    births: Vec<BirthObservation>,
    bundle_live_pods: Vec<String>,
    permits: DrainFenceSource,
}

impl RenderedCutoverPreflight {
    /// Read the render and the observation bundle.
    ///
    /// # Errors
    ///
    /// The render is missing, unparseable or empty; the bundle is invalid; a
    /// catalog row declares a protocol that is not one of the two wire forms.
    /// Every one of these is *unevaluable*, never "clean".
    pub fn load(sources: &PreflightSources, permits: DrainFenceSource) -> Result<Self, String> {
        let manifests = load_manifests(&sources.manifests_path)?;
        let observations = load_observations(sources.observations_path.as_deref())?;
        let catalog = observations
            .catalog
            .into_iter()
            .map(|image| {
                let declared = match image.declared.as_deref() {
                    None => None,
                    Some(raw) => {
                        Some(LauncherAuthorityProtocol::from_str(raw).map_err(|error| {
                            format!("catalog image {:?}: {error}", image.pull_ref)
                        })?)
                    }
                };
                Ok(CatalogImage {
                    pull_ref: image.pull_ref,
                    declared,
                    digest: image.digest,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let births = observations
            .births
            .into_iter()
            .map(|birth| BirthObservation {
                pod: birth.pod,
                target_cpu: birth.target_cpu,
            })
            .collect();
        Ok(Self {
            manifests,
            catalog,
            births,
            bundle_live_pods: observations.live_task_run_pods,
            permits,
        })
    }

    /// Assemble the input and hand it to [`run`].
    ///
    /// `additional_live_pods` is the apiserver half of the drain fence for
    /// callers that can enumerate it themselves (the server-side cutover driver
    /// does; a deploy step reads it from the bundle). It is UNIONED with the
    /// bundle's list rather than replacing it, so neither source can launder the
    /// other's non-empty answer into an empty fence.
    ///
    /// # Errors
    ///
    /// Unevaluable: the rendered config arms the cgroup launcher without the
    /// task-run RuntimeClass, which is `render-gate`'s subject and refuses
    /// there. Distinct from a blocked verdict, which is carried inside
    /// [`Judgement`].
    pub async fn judge(
        &self,
        mode: LauncherAuthorityProtocol,
        additional_live_pods: &[String],
    ) -> Result<Judgement, String> {
        let config = KubernetesConfig::from_env();
        // `build_task_run_job` asserts this pairing. It is `render-gate`'s
        // subject, not this one's, so refuse to evaluate rather than panic in a
        // deploy step.
        if config.cgroup_launcher_mode.renders_sidecar() && !config.task_run_cgroup_writable_enabled
        {
            return Err(
                "the rendered config arms the cgroup launcher without the task-run RuntimeClass; \
                 deploy/preflight/render-gate.sh is the gate for that pairing and refuses it \
                 already"
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
        let ceiling_render_error =
            apply_launcher_authority_protocol(&mut job, config.cgroup_launcher_mode, mode)
                .err()
                .map(|error| {
                    format!(
                        "the dispatch render refused to apply the {mode} authority protocol: \
                         RenderValidationError::{error:?}: {error}"
                    )
                });

        // The production resolver, not the bundle: an inventory that could be
        // handed to the gate by the thing being gated is not an inventory.
        let inventory = LegacyDigestInventory::from_env();

        let mut live_pods = self.bundle_live_pods.clone();
        live_pods.extend(additional_live_pods.iter().cloned());
        live_pods.sort();
        live_pods.dedup();
        let drain = match &self.permits {
            DrainFenceSource::Repository(permits) => observe_drain_fence(permits, live_pods).await,
            DrainFenceSource::Unobservable(reason) => {
                DrainFenceObservation::unobservable(reason.clone())
            }
        };

        let input = CutoverPreflightInput {
            manifests: &self.manifests,
            task_run_job: &job,
            authority_mode: mode,
            catalog: &self.catalog,
            legacy_digest_inventory: &inventory,
            births: &self.births,
            drain: &drain,
        };
        let summary = summarize(&input);
        let verdict = run(&input);
        Ok(Judgement {
            summary,
            ceiling_render_error,
            verdict,
        })
    }
}

/// What the preflight decided about one deployment, at one mode.
///
/// Carries `ceiling_render_error` separately from `verdict` because the render
/// seam's own refusal is not produced by [`run`] — it is produced by the same
/// `apply_launcher_authority_protocol` call dispatch makes, and it belongs to
/// the `launcher-cpu-ceiling` class.
pub struct Judgement {
    /// One-line description of what was judged, for the operator's log.
    pub summary: String,
    /// The dispatch render refused to apply the protocol. Blocking.
    pub ceiling_render_error: Option<String>,
    /// [`run`]'s own verdict.
    pub verdict: Result<crate::cutover_preflight::Report, Blocked>,
}

impl Judgement {
    /// May the cutover proceed?
    ///
    /// Both halves must be clean. A caller that consulted only `verdict` would
    /// flip a deployment whose dispatch render cannot apply the protocol it is
    /// being flipped to.
    #[must_use]
    pub fn is_clear(&self) -> bool {
        self.ceiling_render_error.is_none() && self.verdict.is_ok()
    }

    /// Every blocking class, sorted and deduped, as stable labels.
    #[must_use]
    pub fn blocking_classes(&self) -> Vec<String> {
        let mut classes: Vec<String> = match &self.verdict {
            Ok(_) => Vec::new(),
            Err(blocked) => blocked.classes().iter().map(ToString::to_string).collect(),
        };
        if self.ceiling_render_error.is_some() {
            classes.push(DefectClass::LauncherCpuCeiling.as_str().to_string());
        }
        classes.sort();
        classes.dedup();
        classes
    }

    /// Every blocking reason as `class detail`, in class order.
    #[must_use]
    pub fn blocking_defects(&self) -> Vec<String> {
        let mut lines: Vec<String> = Vec::new();
        if let Some(detail) = &self.ceiling_render_error {
            lines.push(format!("{} {detail}", DefectClass::LauncherCpuCeiling));
        }
        if let Err(blocked) = &self.verdict {
            lines.extend(
                blocked
                    .defects()
                    .iter()
                    .map(|defect| format!("{} {}", defect.class(), defect.detail())),
            );
        }
        lines
    }

    /// The classes [`run`] actually evaluated, as stable labels. Empty when the
    /// verdict is blocked — "no defects" and "no checks ran" must stay
    /// distinguishable.
    #[must_use]
    pub fn evaluated_classes(&self) -> Vec<String> {
        match &self.verdict {
            Ok(report) => report.evaluated().iter().map(ToString::to_string).collect(),
            Err(_) => Vec::new(),
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

fn load_observations(path: Option<&str>) -> Result<Observations, String> {
    let Some(path) = path else {
        return Ok(Observations::default());
    };
    if path.trim().is_empty() {
        return Ok(Observations::default());
    }
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read observations {path}: {error}"))?;
    serde_json::from_str(&raw).map_err(|error| format!("observations {path} are invalid: {error}"))
}

/// The protocol the deployment is being flipped to. Fail-closed on a malformed
/// value; absent means the pre-cutover status quo, which is `leaf-v1`.
///
/// # Errors
///
/// `DJINN_CUTOVER_AUTHORITY_MODE` holds something that is not a launcher
/// authority protocol. Defaulting it would answer a question about `resize-v2`
/// readiness with a `leaf-v1` verdict.
pub fn authority_mode_from_env() -> Result<LauncherAuthorityProtocol, String> {
    match std::env::var("DJINN_CUTOVER_AUTHORITY_MODE") {
        Err(_) => Ok(LauncherAuthorityProtocol::LeafV1),
        Ok(raw) if raw.trim().is_empty() => Ok(LauncherAuthorityProtocol::LeafV1),
        Ok(raw) => LauncherAuthorityProtocol::from_str(raw.trim())
            .map_err(|error| format!("DJINN_CUTOVER_AUTHORITY_MODE: {error}")),
    }
}
