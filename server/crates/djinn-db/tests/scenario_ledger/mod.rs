//! A scenario ledger that records checks, not claims.
//!
//! Two typed-evidence fixtures (`typed_evidence_retry_v1.json` and
//! `typed_evidence_lifecycle_v1.json`) declare a list of scenario names, and
//! their tests compare that list against the scenarios they claim to have
//! proven. The comparison is only worth something if a scenario can be recorded
//! **exclusively** by the assertion that proves it.
//!
//! It could not be. Both ledgers were a plain `Vec` with
//! `proven_scenarios.push("occupied_slot")` written *next to* the assertion.
//! Adversarial verification of proposal `667e` replaced two real assertions in
//! `typed_evidence_retry_v1.rs` with the bare calls they wrapped, kept the
//! pushes, and `cargo test -p djinn-db typed_evidence_retry_v1` stayed **green**.
//! A ledger that survives the deletion of what it claims to record is worse
//! than no ledger: it reports coverage that is absent.
//!
//! So the vector here is private to this module and there is no `push`. Every
//! entry arrives through an assertion; delete the assertion and the entry goes
//! with it, and the declared-vs-proven comparison reddens.

// Shared by two integration-test binaries; each uses a subset of the API, so
// the unused half is dead code from that binary's point of view.
#![allow(dead_code)]

use std::fmt::Debug;

/// Scenarios proven by assertions that actually ran.
pub struct ProvenScenarios(Vec<&'static str>);

impl ProvenScenarios {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// The call must have failed. Records `scenario` only if it did.
    pub fn refuses<T, E>(&mut self, scenario: &'static str, outcome: Result<T, E>, why: &str) {
        assert!(outcome.is_err(), "{scenario}: {why}");
        self.0.push(scenario);
    }

    /// The call must have succeeded. Records `scenario` only if it did.
    pub fn accepts<T, E: Debug>(
        &mut self,
        scenario: &'static str,
        outcome: Result<T, E>,
        why: &str,
    ) -> T {
        match outcome {
            Ok(value) => {
                self.0.push(scenario);
                value
            }
            Err(err) => panic!("{scenario}: {why} (failed with {err:?})"),
        }
    }

    /// `actual` must equal `expected`. Records `scenario` only if it did.
    pub fn observes<T: PartialEq + Debug>(
        &mut self,
        scenario: &'static str,
        actual: T,
        expected: T,
        why: &str,
    ) {
        assert_eq!(actual, expected, "{scenario}: {why}");
        self.0.push(scenario);
    }

    /// The scenario names proven, sorted and deduplicated, ready to compare
    /// against the fixture's declared list.
    pub fn into_sorted(self) -> Vec<String> {
        let mut proven: Vec<String> = self.0.into_iter().map(str::to_owned).collect();
        proven.sort();
        proven.dedup();
        proven
    }
}

/// Compare a fixture's declared scenario list against what the body proved.
///
/// Rejects a fixture that declares the same scenario twice — otherwise a
/// duplicate would hide a scenario the body never proved.
pub fn assert_ledger_reconciles(proven: Vec<String>, declared_scenarios: &[String]) {
    let mut declared = declared_scenarios.to_vec();
    declared.sort();
    declared.dedup();
    assert_eq!(
        declared.len(),
        declared_scenarios.len(),
        "the fixture must not declare a scenario twice",
    );
    assert_eq!(
        proven, declared,
        "every scenario the fixture declares must be proven by this body, and every scenario \
         proven here must be declared by the fixture",
    );
}
