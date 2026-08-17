//! Typed-evidence structural gate: the one place the repository's unresolved
//! typed finding is turned into a transition refusal.
//!
//! Five transitions consume this — Judge verdict, human refinement acceptance,
//! sign-off, verdict override, and graduation. Three of them
//! (`proposal_signoff`, `proposal_graduate`, and re-entry into `in_review`)
//! reach it through [`super::signoff::evaluate_composed_gate`]; the other two
//! (`proposal_debate_append` for a Judge verdict, `proposal_refinement_resolve`,
//! and `proposal_verdict_override`) call [`evaluate_typed_evidence_gate`]
//! directly because they must not inherit the composed gate's DoR checks.
//!
//! ## Why the mode is a parameter
//!
//! [`TypedEvidenceGateMode`] is resolved **once, at the MCP tool boundary**,
//! and passed down. It is deliberately not read from the environment inside the
//! gate: `cargo test` runs a target's tests in one process, so a process-global
//! toggle would make one test's rollout stage leak into another's and flake
//! under parallel execution.

use djinn_db::{
    ProposalRepository, TypedEvidenceParityProbe, TypedEvidenceRepository,
    UnresolvedTypedEvidenceProjection,
};

/// Environment variable consulted once per MCP tool call to pick the stage.
pub const TYPED_EVIDENCE_GATE_ENV: &str = "DJINN_TYPED_EVIDENCE_GATE";

/// Reason code emitted whenever the typed and legacy evidence authorities
/// disagree. The gate fails closed on this in every mode, including `Off`:
/// a disagreement means neither authority can be trusted to admit a
/// transition, which is a different claim from "typed evidence is not live
/// yet".
pub const PARITY_MISMATCH_REASON: &str = "typed_evidence_parity_mismatch";

/// Rollout stage for the typed-evidence structural gate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TypedEvidenceGateMode {
    /// The projection is not consulted and never surfaces. Only the parity
    /// probe runs, so mixed-version drift still fails closed.
    #[default]
    Off,
    /// The projection is read and surfaced on `proposal_show`, but an
    /// unresolved finding does not block a transition.
    Shadow,
    /// An unresolved typed finding blocks the transition.
    Enforce,
}

impl TypedEvidenceGateMode {
    /// Resolve the stage from [`TYPED_EVIDENCE_GATE_ENV`].
    ///
    /// Call this at the MCP tool boundary and pass the result down. Unset or
    /// unrecognized values stay [`TypedEvidenceGateMode::Off`], so a typo in a
    /// deployment cannot silently arm the gate.
    pub fn from_env() -> Self {
        std::env::var(TYPED_EVIDENCE_GATE_ENV)
            .as_deref()
            .map_or(Self::Off, Self::parse)
    }

    /// The whole of the stage vocabulary. Anything unrecognized is `Off`, so a
    /// typo in a deployment cannot silently arm the gate.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "shadow" => Self::Shadow,
            "enforce" => Self::Enforce,
            _ => Self::Off,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }
}

/// What the typed-evidence gate concluded for one proposal.
#[derive(Clone, Debug)]
pub(crate) struct TypedEvidenceGateOutcome {
    pub mode: TypedEvidenceGateMode,
    /// The unresolved finding, when one exists and the mode surfaces it.
    pub projection: Option<UnresolvedTypedEvidenceProjection>,
    /// Set when typed and legacy authority disagree, or when the repository
    /// read itself failed. Both are fail-closed conditions in every mode.
    pub fail_closed_reason: Option<String>,
    /// The refusal string a blocked transition returns.
    pub failure: Option<String>,
}

impl TypedEvidenceGateOutcome {
    /// Nothing was read and nothing blocks.
    fn clear(mode: TypedEvidenceGateMode) -> Self {
        Self {
            mode,
            projection: None,
            fail_closed_reason: None,
            failure: None,
        }
    }

    /// Whether this outcome refuses the transition.
    pub fn blocks(&self) -> bool {
        self.failure.is_some()
    }

    /// Whether the gate has anything at all to report to `proposal_show`.
    pub fn is_reportable(&self) -> bool {
        self.projection.is_some() || self.fail_closed_reason.is_some()
    }
}

/// Render the refusal for an unresolved typed finding.
///
/// The four diagnostics are load-bearing: a Judge reading the refusal has to be
/// able to find the finding, see the claim it is holding open, know which
/// lifecycle state it is stuck in, and know which revision raised it.
fn unresolved_failure(projection: &UnresolvedTypedEvidenceProjection) -> String {
    let mut failure = format!(
        "unresolved typed evidence finding {} (lifecycle: {}; demanded against revision {}; claim: {})",
        projection.finding_id,
        projection.lifecycle.as_str(),
        projection.demanded_revision_seq,
        projection.claim,
    );
    if let Some(outcome) = projection.evidence_outcome {
        failure.push_str(&format!("; evidence outcome: {outcome:?}").to_lowercase());
    }
    if let Some(detail) = projection.failure_detail.as_deref() {
        failure.push_str(&format!("; detail: {detail}"));
    }
    failure
}

/// Evaluate the typed-evidence gate for one proposal.
///
/// Fail-closed order matters. The parity probe runs first and in every mode: a
/// disagreement between typed and legacy authority means the projection cannot
/// be trusted either way, so no mode may admit the transition. Only once parity
/// holds does the mode decide whether an unresolved finding blocks.
pub(crate) async fn evaluate_typed_evidence_gate(
    repo: &ProposalRepository,
    proposal_id: &str,
    mode: TypedEvidenceGateMode,
) -> TypedEvidenceGateOutcome {
    let typed = TypedEvidenceRepository::new(repo.db().clone());
    let probe = match typed.legacy_parity_probe(proposal_id).await {
        Ok(probe) => probe,
        Err(error) => {
            // An unreadable authority is indistinguishable from a mismatched
            // one for the purposes of admitting a transition.
            let reason = format!("{PARITY_MISMATCH_REASON}: probe unavailable ({error})");
            return TypedEvidenceGateOutcome {
                mode,
                projection: None,
                fail_closed_reason: Some(reason.clone()),
                failure: Some(reason),
            };
        }
    };
    if let TypedEvidenceParityProbe::Mismatch(reason) = probe {
        let reason = format!("{PARITY_MISMATCH_REASON}: {}", reason.as_str());
        return TypedEvidenceGateOutcome {
            mode,
            projection: None,
            fail_closed_reason: Some(reason.clone()),
            failure: Some(reason),
        };
    }

    // `Off` reads nothing further. This is what keeps a pre-rollout deployment
    // byte-identical to the gate as it stood before typed evidence existed.
    if mode == TypedEvidenceGateMode::Off {
        return TypedEvidenceGateOutcome::clear(mode);
    }

    let projection = match typed.unresolved_projection(proposal_id).await {
        Ok(projection) => projection,
        Err(error) => {
            let reason = format!("{PARITY_MISMATCH_REASON}: projection unavailable ({error})");
            return TypedEvidenceGateOutcome {
                mode,
                projection: None,
                fail_closed_reason: Some(reason.clone()),
                failure: Some(reason),
            };
        }
    };
    let Some(projection) = projection else {
        return TypedEvidenceGateOutcome::clear(mode);
    };
    let failure = (mode == TypedEvidenceGateMode::Enforce).then(|| unresolved_failure(&projection));
    TypedEvidenceGateOutcome {
        mode,
        projection: Some(projection),
        fail_closed_reason: None,
        failure,
    }
}

/// Refuse a transition that does not run the full composed gate.
///
/// Judge verdicts, human refinement acceptance, and verdict override each have
/// their own preconditions and must not inherit DoR blocking, but they are
/// still structural transitions over the same evidence authority.
pub(crate) async fn typed_evidence_transition_refusal(
    repo: &ProposalRepository,
    proposal_id: &str,
    mode: TypedEvidenceGateMode,
) -> Option<String> {
    evaluate_typed_evidence_gate(repo, proposal_id, mode)
        .await
        .failure
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrecognized_modes_stay_off() {
        // `parse` is the whole of `from_env`'s decision, so this covers the
        // real mapping without touching the process environment — a shared env
        // var is exactly the flake this parameterized design exists to avoid.
        for (raw, expected) in [
            ("off", TypedEvidenceGateMode::Off),
            ("shadow", TypedEvidenceGateMode::Shadow),
            ("SHADOW", TypedEvidenceGateMode::Shadow),
            ("  Enforce ", TypedEvidenceGateMode::Enforce),
            ("enforce!", TypedEvidenceGateMode::Off),
            ("1", TypedEvidenceGateMode::Off),
            ("", TypedEvidenceGateMode::Off),
        ] {
            assert_eq!(TypedEvidenceGateMode::parse(raw), expected, "for {raw:?}");
        }
    }

    #[test]
    fn default_mode_is_off() {
        assert_eq!(TypedEvidenceGateMode::default(), TypedEvidenceGateMode::Off);
    }
}
