//! **The operator entry point for the launcher-authority cutover** (proposal
//! `3i92`, epic `xowm`, task `eeky`).
//!
//! # Why this module exists
//!
//! [`crate::task_run_resize_rollout::ResizeRollout`] encodes the whole fenced
//! transition — the ordering, the two drain checks, the compare-and-swap, the
//! reverse that can refuse itself — and until this module landed
//! `ResizeRollout::production` had **zero callers in any binary**. The staged
//! activation existed as a library with tests and could not be run by anyone.
//! That is not a smaller problem than a bug; it is the problem this epic keeps
//! rediscovering, and `scripts/check-resize-reachability.sh` now guards this
//! call site the way it guards the others.
//!
//! # Why a `bin/`, and not an admin MCP tool
//!
//! Three properties decided it, and each of them an MCP tool fails:
//!
//! 1. **The verdict must depend on the RENDER, not on the operator's shell.**
//!    The preflight's Rust half re-renders the task-run Job from `DJINN_K8S_*`,
//!    so those variables have to come from the *rendered* `djinn-server`
//!    container. `deploy/preflight/cutover-preflight.sh` already extracts them
//!    and re-execs under `env -i`; running the cutover through that same
//!    wrapper is what makes the flip and the deploy gate answer the same
//!    question. A tool executing inside a running `djinn-server` reads that
//!    server's environment — which is the *old* deployment's, and the whole
//!    point of step 3 is that the new server is deployed while the mode has not
//!    moved.
//! 2. **Step 3 deploys a new server; steps 4-9 must survive it.** An entry
//!    point hosted by the process being replaced cannot run a sequence that
//!    replaces it.
//! 3. **The refusal is the product.** The exit codes are the contract a deploy
//!    lane reads — `0` flipped, `1` blocked, `2` unevaluable — the same triple
//!    `cutover-preflight` and `render-gate` already speak.
//!
//! # What an operator runs
//!
//! ```text
//! DJINN_CUTOVER_DIRECTION=activate \
//! DJINN_CUTOVER_PLAN=/path/to/plan.json \
//! DJINN_CUTOVER_AUTHORITY_MODE=resize-v2 \
//! DJINN_DATABASE_URL=postgres://... \
//!   deploy/cutover/authority-cutover.sh deploy/helm/djinn --values prod-values.yaml
//! ```
//!
//! and to reverse it, `DJINN_CUTOVER_DIRECTION=rollback` with
//! `DJINN_CUTOVER_AUTHORITY_MODE=leaf-v1`. See
//! `docs/deploy/launcher-authority-cutover.md`.

use std::sync::Arc;

use djinn_core::events::EventBus;
use djinn_db::Database;
use djinn_k8s::cutover_preflight_driver::PreflightSources;
use djinn_k8s::runtime::KubernetesRuntime;
use djinn_launcher_protocol::LauncherAuthorityProtocol;
use serde::Deserialize;

use crate::task_run_resize_rollout::{
    AdmissionControl as _, DispatchProbe, DurableAdmissionControl, ResizeRollout, RetainedArtifact,
    RetentionRole, RolloutBlocked, RolloutPlan, RolloutStep,
};

/// Which way the cutover goes.
///
/// Parsed, never defaulted. A missing or malformed direction is refused: an
/// operator who meant `rollback` and got `activate` because a variable was
/// unset would flip a deployment the wrong way at 03:00.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutoverDirection {
    /// `leaf-v1` → `resize-v2`.
    Activate,
    /// `resize-v2` → `leaf-v1`.
    Rollback,
}

impl CutoverDirection {
    /// Parse a wire value.
    ///
    /// # Errors
    ///
    /// Anything but `activate` or `rollback`.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "activate" => Ok(Self::Activate),
            "rollback" => Ok(Self::Rollback),
            other => Err(format!(
                "DJINN_CUTOVER_DIRECTION must be `activate` or `rollback`, got {other:?}"
            )),
        }
    }

    /// The authority mode this direction targets.
    #[must_use]
    pub const fn target(self) -> LauncherAuthorityProtocol {
        match self {
            Self::Activate => LauncherAuthorityProtocol::ResizeV2,
            Self::Rollback => LauncherAuthorityProtocol::LeafV1,
        }
    }

    /// Stable label for logs and the exit-status contract.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::Rollback => "rollback",
        }
    }
}

/// The operator's plan file: everything one cutover run needs that a render
/// cannot contain.
///
/// `deny_unknown_fields` because a typo'd `retained` key that silently produced
/// an empty retained set would make step 5 pass vacuously — a cutover proving
/// retention of nothing.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CutoverPlanDocument {
    /// The authority epoch this cutover was planned against. Every flip is a
    /// compare-and-swap against it.
    pub expected_epoch: i64,
    /// OCI registry base URL, e.g. `https://ghcr.io`.
    pub registry_base_url: String,
    /// Operator-facing pause reason, written onto the durable pause row.
    pub reason: String,
    /// The task-run id the pause proves itself against by attempting a
    /// dispatch. A real id, so the probe travels the path a task run takes.
    pub probe_task_run_id: String,
    /// `images.id` of the catalog row the probe dispatch would run.
    pub probe_image_id: String,
    /// Every artifact whose continued pullability this cutover depends on.
    pub retained: Vec<RetainedArtifactDocument>,
}

/// One retained artifact, as the plan file spells it.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedArtifactDocument {
    /// `images.id`, for the operator's report.
    pub image_id: String,
    /// The registry repository path, e.g. `djinnos/djinn-image-i1`.
    pub repository: String,
    /// The immutable manifest digest the catalog records.
    pub digest: String,
    /// `legacy-no-handshake`, `leaf-v1-rollback` or `resize-v2-current`.
    pub role: String,
}

impl RetainedArtifactDocument {
    /// Convert to the driver's type.
    ///
    /// # Errors
    ///
    /// The role is not one of the three. Parsed, never defaulted: a typo that
    /// silently became `ResizeV2Current` would misreport what a blocked
    /// retention check was protecting.
    pub fn parse(&self) -> Result<RetainedArtifact, String> {
        let role = match self.role.trim() {
            "legacy-no-handshake" => RetentionRole::LegacyNoHandshake,
            "leaf-v1-rollback" => RetentionRole::LeafV1Rollback,
            "resize-v2-current" => RetentionRole::ResizeV2Current,
            other => {
                return Err(format!(
                    "retained artifact {}: role must be one of legacy-no-handshake, \
                     leaf-v1-rollback, resize-v2-current; got {other:?}",
                    self.image_id
                ));
            }
        };
        Ok(RetainedArtifact {
            image_id: self.image_id.clone(),
            repository: self.repository.clone(),
            digest: self.digest.clone(),
            role,
        })
    }
}

impl CutoverPlanDocument {
    /// Read and parse a plan file.
    ///
    /// # Errors
    ///
    /// The file is missing, is not JSON, carries an unknown key, or names a
    /// role that is not one of the three.
    pub fn load(path: &str) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read cutover plan {path}: {error}"))?;
        let document: Self = serde_json::from_str(&raw)
            .map_err(|error| format!("cutover plan {path} is invalid: {error}"))?;
        if document.retained.is_empty() {
            return Err(format!(
                "cutover plan {path} retains nothing; step 5 would prove the pullability of an \
                 empty set, which is not evidence that anything is still runnable"
            ));
        }
        for artifact in &document.retained {
            artifact.parse()?;
        }
        Ok(document)
    }
}

/// One whole cutover request.
pub struct CutoverRequest {
    /// Which way.
    pub direction: CutoverDirection,
    /// A LIVE `helm template` render, converted to JSON.
    pub rendered_manifests_path: String,
    /// The parsed plan.
    pub plan: CutoverPlanDocument,
    /// Recorded on the durable pause row this cutover writes.
    pub paused_by: String,
}

impl CutoverRequest {
    /// Assemble a request from the environment the wrapper hands the binary,
    /// plus the render it passes as `argv[1]`.
    ///
    /// # Errors
    ///
    /// A missing or malformed `DJINN_CUTOVER_DIRECTION`, a missing
    /// `DJINN_CUTOVER_PLAN`, or an unusable plan file.
    pub fn from_env(rendered_manifests_path: impl Into<String>) -> Result<Self, String> {
        let direction = CutoverDirection::parse(
            &std::env::var("DJINN_CUTOVER_DIRECTION")
                .map_err(|_| "DJINN_CUTOVER_DIRECTION is not set".to_string())?,
        )?;
        let plan_path = std::env::var("DJINN_CUTOVER_PLAN")
            .map_err(|_| "DJINN_CUTOVER_PLAN is not set".to_string())?;
        Ok(Self {
            direction,
            rendered_manifests_path: rendered_manifests_path.into(),
            plan: CutoverPlanDocument::load(&plan_path)?,
            paused_by: std::env::var("DJINN_CUTOVER_PAUSED_BY")
                .unwrap_or_else(|_| "authority-cutover".to_string()),
        })
    }
}

/// What one cutover run did.
pub struct CutoverReport {
    /// The steps that actually completed, in the order they completed.
    pub journal: Vec<RolloutStep>,
    /// The authority epoch after the flip.
    pub epoch: i64,
    /// Pods created between the pause and the resume. Structurally zero; read
    /// back and reported so its absence is observable rather than assumed.
    pub dispatches_admitted_while_paused: u64,
}

/// Why a cutover run did not complete.
#[derive(Debug, thiserror::Error)]
pub enum CutoverFailure {
    /// The request, the plan or the preflight sources could not be assembled.
    /// Nothing was attempted.
    #[error("the cutover could not be evaluated: {0}")]
    Unevaluable(String),
    /// The rollout refused, at the step named by `journal`'s tail.
    #[error("the cutover was blocked: {blocked}")]
    Blocked {
        /// The block.
        #[source]
        blocked: RolloutBlocked,
        /// How far it got. Steps that returned an error never appear here.
        journal: Vec<RolloutStep>,
        /// Whether admission is left paused, evaluated against the SAME durable
        /// predicate the coordinator's dispatch loop guards on — not inferred
        /// from the journal. A rollback that blocks before its own pause step
        /// still has to report a pause an earlier operator left behind, and a
        /// journal cannot know about one.
        admission_left_paused: bool,
    },
}

impl CutoverFailure {
    /// The exit status a deploy lane reads: `1` blocked, `2` unevaluable.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::Blocked { .. } => 1,
            Self::Unevaluable(_) => 2,
        }
    }
}

/// **Run one cutover, forward or reverse.**
///
/// This is the function the binary's `main` is a shell around, and the function
/// the integration suite drives. It composes
/// [`ResizeRollout::production`] — the real dispatch-pause state and the
/// coordinator's own refusal predicate, the real apiserver-backed Pod plane, a
/// real HTTP registry probe, and the real deploy-time preflight over the
/// render — and then calls [`ResizeRollout::activate`] or
/// [`ResizeRollout::rollback`]. It re-implements no step and skips none: the
/// ordering, both drain checks and the preflight gate all belong to
/// `ResizeRollout`, and this function cannot reach past them.
///
/// # Errors
///
/// [`CutoverFailure::Unevaluable`] when the rollout could not even be composed
/// (an unreadable render, an unresolvable probe image), and
/// [`CutoverFailure::Blocked`] carrying the refusal and the journal. In every
/// blocked case the authority mode is unchanged, and for every block at or
/// after the pause, admission is left paused — `rollback`'s allowlist and
/// retention checks run before the pause precisely so a rollback that cannot
/// find its artifacts blocks without having touched anything.
pub async fn run(
    db: Database,
    events: EventBus,
    runtime: Arc<KubernetesRuntime>,
    request: &CutoverRequest,
) -> Result<CutoverReport, CutoverFailure> {
    let retained = request
        .plan
        .retained
        .iter()
        .map(RetainedArtifactDocument::parse)
        .collect::<Result<Vec<_>, String>>()
        .map_err(CutoverFailure::Unevaluable)?;

    let sources = PreflightSources::from_env(request.rendered_manifests_path.clone());
    let rollout = ResizeRollout::production(
        db.clone(),
        events.clone(),
        runtime,
        &request.plan.registry_base_url,
        &request.paused_by,
        &sources,
    )
    .map_err(CutoverFailure::Unevaluable)?;

    // The probe is a REAL catalog row, resolved through the production
    // accessor. A synthesised `Image` would let the pause step prove itself
    // against an artifact no project can dispatch, which is not the path a task
    // run takes.
    let image = djinn_db::ImageRepository::new(db.clone())
        .get(&request.plan.probe_image_id)
        .await
        .map_err(|error| {
            CutoverFailure::Unevaluable(format!(
                "cannot read probe image {}: {error}",
                request.plan.probe_image_id
            ))
        })?
        .ok_or_else(|| {
            CutoverFailure::Unevaluable(format!(
                "probe image {} is not in the catalog; the pause step proves itself by \
                 dispatching it",
                request.plan.probe_image_id
            ))
        })?;

    let plan = RolloutPlan {
        retained: &retained,
        probe: DispatchProbe {
            task_run_id: request.plan.probe_task_run_id.clone(),
            image,
        },
        expected_epoch: request.plan.expected_epoch,
        reason: &request.plan.reason,
    };

    let outcome = match request.direction {
        CutoverDirection::Activate => rollout.activate(&plan).await,
        CutoverDirection::Rollback => rollout.rollback(&plan).await,
    };

    match outcome {
        Ok(epoch) => Ok(CutoverReport {
            journal: rollout.journal(),
            epoch,
            dispatches_admitted_while_paused: rollout.dispatches_admitted_while_paused(),
        }),
        Err(blocked) => {
            // Read the production dispatch-pause predicate, not the journal:
            // `rollback`'s allowlist and retention checks run BEFORE its own
            // pause step, so a rollback blocked there has an empty journal and
            // may still be sitting on a pause a previous, half-finished cutover
            // left behind. Reporting "untouched" there would tell an operator
            // that dispatch had resumed when it had not.
            //
            // Unreadable is reported as PAUSED. The alternative — reporting
            // "not paused" because the read failed — is the one answer that
            // could make somebody stop looking.
            let admission_left_paused =
                DurableAdmissionControl::new(db, events, &request.paused_by)
                    .dispatch_is_paused()
                    .await
                    .unwrap_or(true);
            Err(CutoverFailure::Blocked {
                blocked,
                journal: rollout.journal(),
                admission_left_paused,
            })
        }
    }
}
