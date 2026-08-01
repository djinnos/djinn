//! Behavioral fixture contract for terminal linked evidence lifecycle cases.

#[cfg(test)]
mod tests {
    use djinn_db::repositories::proposal::EvidenceDerivedOutcome;
    use serde::Deserialize;

    const LIFECYCLE: &str = include_str!("../tests/fixtures/evidence_lifecycle_cases.json");

    #[derive(Deserialize)]
    struct Fixture {
        cases: Vec<Case>,
        idempotency: Vec<String>,
    }

    #[derive(Deserialize)]
    struct Case {
        name: String,
        structured_completion: Option<String>,
        terminal_success: bool,
        resume_refinement: bool,
    }

    /// Keep the fixture closed over the production terminal vocabulary. The
    /// database lifecycle tests drive the actual persistence primitive while
    /// coordinator recovery tests drive both resume paths.
    #[test]
    fn evidence_rollout_contract() {
        let fixture: Fixture = serde_json::from_str(LIFECYCLE).expect("valid lifecycle fixture");
        assert_eq!(fixture.cases.len(), 6);
        for case in fixture.cases {
            let derived = match case.structured_completion.as_deref() {
                Some("resolved") => Some(EvidenceDerivedOutcome::Resolved),
                Some("partial") => Some(EvidenceDerivedOutcome::Partial),
                Some("unresolved") => Some(EvidenceDerivedOutcome::Unresolved),
                Some("malformed") | Some("missing") | None => None,
                Some(_) => None,
            };
            assert_eq!(case.terminal_success, derived.is_some(), "{} lifecycle", case.name);
            assert_eq!(case.resume_refinement, derived.is_some(), "{} resume", case.name);
        }
        assert_eq!(fixture.idempotency, ["repeated_delivery", "restart_after_persistence", "already_recorded"]);
    }
}
