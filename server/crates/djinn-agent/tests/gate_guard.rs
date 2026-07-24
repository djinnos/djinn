//! Integration tests for the ri23 build-drift soft gate.
//!
//! Run with: `cargo test -p djinn-agent --test gate_guard`
//!
//! * `build_drift_gate_v1` — the full decision table plus deny-once,
//!   inert-with-no-plan, never-gating-the-canonical-command, and bounded
//!   telemetry.
//! * `advisory_steering_v1` — a divergent baseline/candidate replay over
//!   unchanged source states, proving the routing + pass-through thresholds.

use djinn_agent::test_helpers::{
    BuildDriftClassification, BuildDriftIneligibleReason, CanonicalCommand, agent_context_from_db,
    build_drift_shell_gate_for_test, classify_build_drift, create_test_db,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

// ── Fixture shapes ───────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RawCanonical {
    executable: String,
    #[serde(default)]
    argv: Vec<String>,
}

impl RawCanonical {
    fn to_canonical(&self) -> CanonicalCommand {
        CanonicalCommand::new(&self.executable, self.argv.clone())
    }
}

fn canonical_vec(raw: &[RawCanonical]) -> Vec<CanonicalCommand> {
    raw.iter().map(RawCanonical::to_canonical).collect()
}

// ── AC 1: build_drift_gate_v1 ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DecisionCase {
    command: String,
    expect: String,
    #[allow(dead_code)]
    #[serde(default)]
    note: String,
}

#[derive(Debug, Deserialize)]
struct DecisionFixture {
    project_id: String,
    canonical: Vec<RawCanonical>,
    cases: Vec<DecisionCase>,
}

/// Full decision table + deny-once + inert + never-gating-canonical + bounded
/// telemetry.
#[tokio::test]
async fn build_drift_gate_v1() {
    let fixture: DecisionFixture =
        serde_json::from_str(include_str!("fixtures/build_drift_gate_v1.json"))
            .expect("parse build_drift_gate_v1 fixture");
    let canonical = canonical_vec(&fixture.canonical);
    let project = Some(fixture.project_id.as_str());

    // 1. Decision table — pure classification for every enumerated shape.
    for case in &fixture.cases {
        let got = classify_build_drift(&case.command, project, &canonical).describe();
        assert_eq!(
            got, case.expect,
            "command {:?}: expected {:?}, got {:?} ({})",
            case.command, case.expect, got, case.note
        );
    }

    // 2. Bounded telemetry: every case maps to one of exactly five outcome
    //    families, and every ineligible reason is one of exactly ten enumerated
    //    labels — no dynamic strings ever reach a metric label.
    const OUTCOME_LABELS: [&str; 5] = ["argv_equal", "drift", "unrelated", "inert", "ineligible"];
    const INELIGIBLE_REASONS: [&str; 10] = [
        "parse_error",
        "env_assignment",
        "redirection",
        "pipeline",
        "chain",
        "subshell",
        "substitution",
        "background",
        "nested_interpreter",
        "absolute_path",
    ];
    for case in &fixture.cases {
        let label = &case.expect;
        let (family, reason) = match label.split_once(':') {
            Some((fam, rea)) => (fam, Some(rea)),
            None => (label.as_str(), None),
        };
        assert!(
            OUTCOME_LABELS.contains(&family),
            "unbounded outcome family {family:?} from {label:?}"
        );
        if let Some(reason) = reason {
            assert_eq!(family, "ineligible");
            assert!(
                INELIGIBLE_REASONS.contains(&reason),
                "unbounded ineligible reason {reason:?}"
            );
        }
    }
    // The enum's own `as_str()` label set must match the telemetry array exactly.
    for reason in [
        BuildDriftIneligibleReason::ParseError,
        BuildDriftIneligibleReason::EnvAssignment,
        BuildDriftIneligibleReason::Redirection,
        BuildDriftIneligibleReason::Pipeline,
        BuildDriftIneligibleReason::Chain,
        BuildDriftIneligibleReason::Subshell,
        BuildDriftIneligibleReason::Substitution,
        BuildDriftIneligibleReason::Background,
        BuildDriftIneligibleReason::NestedInterpreter,
        BuildDriftIneligibleReason::AbsolutePath,
    ] {
        assert!(
            INELIGIBLE_REASONS.contains(&reason.as_str()),
            "reason {reason:?} not in bounded set"
        );
    }

    // 3. Inert with no plan — an obvious drift command classifies as Inert when
    //    there are no canonical command groups configured.
    assert_eq!(
        classify_build_drift("cargo build -p foo", project, &[]),
        BuildDriftClassification::Inert,
    );

    // 4. Stateful deny-once + repeat-pass + never-gating-the-canonical, driven
    //    through the real per-session gate against an in-memory FileTime.
    let state = agent_context_from_db(create_test_db(), CancellationToken::new());
    let session = "sess-build-drift-gate-v1";
    let drift_cmd = "cargo build -p foo";

    // First divergent build → denied once (steer message).
    let first = build_drift_shell_gate_for_test(&state, session, drift_cmd, project, &canonical)
        .await
        .expect_err("first drift must be denied");
    assert!(
        first.contains("run_verification"),
        "deny message must steer toward run_verification: {first}"
    );
    // Same command again → passes (never denies an equivalent key twice).
    build_drift_shell_gate_for_test(&state, session, drift_cmd, project, &canonical)
        .await
        .expect("repeat of the same drift key must pass");
    build_drift_shell_gate_for_test(&state, session, drift_cmd, project, &canonical)
        .await
        .expect("further repeats must keep passing");

    // A different divergent build is a distinct key → denied once on its own.
    build_drift_shell_gate_for_test(
        &state,
        session,
        "cargo build --release",
        project,
        &canonical,
    )
    .await
    .expect_err("a distinct drift key denies once");

    // Never gates the canonical command: running the exact resolved command via
    // shell is an argv_equal pass, not a deny — this is the closest
    // shell-observable analog of the run_verification tool path, which never
    // routes through the shell handler at all.
    build_drift_shell_gate_for_test(
        &state,
        session,
        "cargo clippy --all-targets -- -D warnings",
        project,
        &canonical,
    )
    .await
    .expect("exact canonical command must pass (never gated)");
    build_drift_shell_gate_for_test(
        &state,
        session,
        "cargo nextest run --workspace",
        project,
        &canonical,
    )
    .await
    .expect("exact canonical command (second group) must pass");

    // Inert path never denies, even for a drift-shaped command.
    build_drift_shell_gate_for_test(&state, session, drift_cmd, project, &[])
        .await
        .expect("inert gate (no plan) must never deny");

    // Per-(session, project) scoping: the same drift command in a *different*
    // session is a fresh first-deny (session dimension is the map key).
    build_drift_shell_gate_for_test(&state, "sess-other", drift_cmd, project, &canonical)
        .await
        .expect_err("a fresh session re-denies the first drift");
    // And a different project id yields a distinct key even in the same session.
    build_drift_shell_gate_for_test(&state, session, drift_cmd, Some("proj-other"), &canonical)
        .await
        .expect_err("a different project id is a distinct key, denied once");
}

// ── AC 2: advisory_steering_v1 ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SteeringState {
    #[allow(dead_code)]
    state_id: String,
    invocations: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SteeringSession {
    session_id: String,
    project_id: String,
    states: Vec<SteeringState>,
}

#[derive(Debug, Deserialize)]
struct SteeringFixture {
    canonical: Vec<RawCanonical>,
    sessions: Vec<SteeringSession>,
}

/// One arm's aggregate outcome over the corpus.
#[derive(Debug, Default)]
struct ArmTally {
    /// Eligible profile-tool invocations (drift or exact-canonical).
    eligible: usize,
    /// Eligible invocations that ended up on the canonical `run_verification`
    /// path (a steered drift, or an exact-canonical run).
    routed: usize,
    /// Compile-producing pass-throughs, per unchanged source state.
    passthrough_per_state: Vec<usize>,
}

impl ArmTally {
    fn routing_rate(&self) -> f64 {
        if self.eligible == 0 {
            0.0
        } else {
            self.routed as f64 / self.eligible as f64
        }
    }

    fn median_passthrough(&self) -> f64 {
        let mut v = self.passthrough_per_state.clone();
        v.sort_unstable();
        if v.is_empty() {
            return 0.0;
        }
        let mid = v.len() / 2;
        if v.len() % 2 == 1 {
            v[mid] as f64
        } else {
            (v[mid - 1] + v[mid]) as f64 / 2.0
        }
    }
}

/// Is an invocation an *eligible profile-tool* invocation? True for drift and
/// exact-canonical classifications; false for unrelated/ineligible/inert.
fn is_eligible(cls: &BuildDriftClassification) -> bool {
    matches!(
        cls,
        BuildDriftClassification::ArgvEqual | BuildDriftClassification::Drift { .. }
    )
}

fn is_argv_equal(cls: &BuildDriftClassification) -> bool {
    matches!(cls, BuildDriftClassification::ArgvEqual)
}

/// Replay the corpus with divergent baseline/candidate behavior over identical
/// inputs, asserting the routing + pass-through thresholds.
#[tokio::test]
async fn advisory_steering_v1() {
    let fixture: SteeringFixture =
        serde_json::from_str(include_str!("fixtures/advisory_steering_v1.json"))
            .expect("parse advisory_steering_v1 fixture");
    let canonical = canonical_vec(&fixture.canonical);

    let mut baseline = ArmTally::default();
    let mut candidate = ArmTally::default();

    for session in &fixture.sessions {
        let project = Some(session.project_id.as_str());
        // Candidate arm holds live per-(session, project) gate state; baseline
        // arm never consults the gate at all.
        let cand_state = agent_context_from_db(create_test_db(), CancellationToken::new());

        for st in &session.states {
            let mut base_state_passthrough = 0usize;
            let mut cand_state_passthrough = 0usize;

            for cmd in &st.invocations {
                let cls = classify_build_drift(cmd, project, &canonical);
                if !is_eligible(&cls) {
                    // Not an eligible profile-tool invocation — excluded from
                    // BOTH arms' denominators identically.
                    continue;
                }
                baseline.eligible += 1;
                candidate.eligible += 1;

                // Baseline: gate is off. An exact-canonical run still routes
                // through canonical; every divergent build is an ad-hoc
                // compile pass-through.
                if is_argv_equal(&cls) {
                    baseline.routed += 1;
                } else {
                    base_state_passthrough += 1;
                }

                // Candidate: replay through the real per-session gate. A deny
                // steers the intent onto run_verification (routed); an
                // exact-canonical run is already canonical (routed); an allowed
                // drift (repeat_pass) is a compile pass-through.
                let decision = build_drift_shell_gate_for_test(
                    &cand_state,
                    &session.session_id,
                    cmd,
                    project,
                    &canonical,
                )
                .await;
                match decision {
                    Err(_) => candidate.routed += 1,
                    Ok(()) if is_argv_equal(&cls) => candidate.routed += 1,
                    Ok(()) => cand_state_passthrough += 1, // repeat_pass drift
                }
            }

            baseline.passthrough_per_state.push(base_state_passthrough);
            candidate.passthrough_per_state.push(cand_state_passthrough);
        }
    }

    // Identical inputs / denominators between arms.
    assert_eq!(
        baseline.eligible, candidate.eligible,
        "arms must share an identical eligible denominator"
    );
    assert!(
        candidate.eligible > 0,
        "corpus must contain eligible invocations"
    );

    // AC threshold 1: >= 90% of eligible profile-tool invocations route through
    // run_verification in the candidate arm.
    assert!(
        candidate.routing_rate() >= 0.90,
        "candidate routing rate {:.4} < 0.90 (routed {} / eligible {})",
        candidate.routing_rate(),
        candidate.routed,
        candidate.eligible
    );

    // AC threshold 2: median compile-producing pass-through <= 1 per unchanged
    // source state in the candidate arm.
    assert!(
        candidate.median_passthrough() <= 1.0,
        "candidate median pass-through per state {:.2} > 1",
        candidate.median_passthrough()
    );

    // The gate must materially improve routing versus the do-nothing baseline
    // (the corpus is divergent, not degenerate).
    assert!(
        candidate.routing_rate() > baseline.routing_rate(),
        "candidate routing {:.4} must exceed baseline {:.4}",
        candidate.routing_rate(),
        baseline.routing_rate()
    );
}
