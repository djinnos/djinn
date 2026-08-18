//! Role-scoped typed tribunal evidence context.
//!
//! The type and its renderer live in `djinn-roles`, which owns [`AgentType`]
//! and the prompt templates, and which both this crate and `djinn-coordinator`
//! depend on — `djinn-agent` depends on `djinn-coordinator`, so the renderer
//! cannot live here and still be reachable from the coordinator that injects
//! it into a task description.
//!
//! This module is the facade the epic's named command reaches:
//! `cargo test -p djinn-agent typed_evidence_context` runs the contract below,
//! and the committed role snapshots live under
//! `crates/djinn-agent/src/snapshots/typed_evidence_context/`.

pub use djinn_roles::typed_evidence_context::*;

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_roles::AgentType;

    /// A fully-populated context, so every optional surface is present and
    /// role scoping is what removes it rather than the data being absent.
    fn full_context() -> TypedEvidenceRoleContextV1 {
        TypedEvidenceRoleContextV1 {
            version: TypedEvidenceRoleContextV1::VERSION.to_owned(),
            finding_id: "01a01188-0000-7000-8000-00000000feed".to_owned(),
            lifecycle: TypedEvidenceContextLifecycle::EvidenceReceived,
            claim: "Can the launcher share a cgroup across pods?".to_owned(),
            demanded_revision_seq: 3,
            planned_checks: vec![
                TypedEvidencePlannedCheckContext {
                    check_id: "cgroup-delegation-probe".to_owned(),
                    method: "command".to_owned(),
                    status: Some("passed".to_owned()),
                    anchor_locator: Some("command:01a01188-0000-7000-8000-0000000000aa".to_owned()),
                    anchor_health: Some(TypedEvidenceContextAnchorHealth::Healthy),
                },
                TypedEvidencePlannedCheckContext {
                    check_id: "launcher-clone-into-cgroup".to_owned(),
                    method: "code".to_owned(),
                    status: Some("failed".to_owned()),
                    anchor_locator: None,
                    anchor_health: Some(TypedEvidenceContextAnchorHealth::Unavailable),
                },
            ],
            attempts: vec![
                TypedEvidenceAttemptContext {
                    sequence: 1,
                    spike_task_id: "01a01188-0000-7000-8000-0000000000b1".to_owned(),
                    outcome: None,
                    failure_detail: Some("payload rejected: missing_identity".to_owned()),
                },
                TypedEvidenceAttemptContext {
                    sequence: 2,
                    spike_task_id: "01a01188-0000-7000-8000-0000000000b2".to_owned(),
                    outcome: Some("partial".to_owned()),
                    failure_detail: None,
                },
            ],
            evidence_outcome: Some("partial".to_owned()),
            gaps: vec![
                "launcher-clone-into-cgroup: could not reach a host with delegation enabled"
                    .to_owned(),
            ],
            demand: Some(TypedEvidenceDemandContext {
                category: "feasibility".to_owned(),
                load_bearing: true,
                threshold: "The spec cannot be accepted while it is unknown whether the \
                            kernel permits the delegation this design assumes."
                    .to_owned(),
                expected_checks: vec![
                    "Run the delegation probe on a representative host".to_owned(),
                    "Read the launcher's cgroup attach path".to_owned(),
                ],
            }),
            retry: Some(TypedEvidenceRetryContext {
                attempts_used: 2,
                attempts_allowed: 2,
                last_failure: Some("payload rejected: missing_identity".to_owned()),
                conflict_code: Some("active_evidence_conflict".to_owned()),
            }),
            disposition: Some(TypedEvidenceDispositionContext {
                disposition: "resolved".to_owned(),
                folding_revision: 4,
                rationale: "The probe settles the delegation question for the supported hosts."
                    .to_owned(),
                withdrawal_is_non_load_bearing: false,
            }),
        }
    }

    /// AC2 — the serialized field set is exactly the pinned list.
    ///
    /// This is the checkable form of "no prose/QuestionForm inventory": the
    /// type carries a single `claim` string and no free-text question
    /// collection, so an inventory cannot be smuggled in without appearing
    /// here first.
    #[test]
    fn typed_evidence_context_shape_is_closed() {
        // One shared implementation, so `typed_evidence_demand` cannot drift
        // from this contract.
        djinn_roles::typed_evidence_context::assert_closed_shape(&full_context());
    }

    #[test]
    fn typed_evidence_context_advocate_snapshot() {
        insta::with_settings!({ snapshot_path => "snapshots/typed_evidence_context" }, {
            insta::assert_snapshot!(render_for_role(AgentType::Advocate, &full_context()));
        });
    }

    #[test]
    fn typed_evidence_context_adversary_snapshot() {
        insta::with_settings!({ snapshot_path => "snapshots/typed_evidence_context" }, {
            insta::assert_snapshot!(render_for_role(AgentType::Adversary, &full_context()));
        });
    }

    #[test]
    fn typed_evidence_context_judge_snapshot() {
        insta::with_settings!({ snapshot_path => "snapshots/typed_evidence_context" }, {
            insta::assert_snapshot!(render_for_role(AgentType::Judge, &full_context()));
        });
    }

    /// AC1 — the three renderings differ only in the role-scoped sections.
    ///
    /// Stripping the three sections must leave three byte-identical strings, so
    /// no role can be told a different version of the shared facts.
    #[test]
    fn typed_evidence_context_roles_share_one_body() {
        fn shared_prefix(rendered: &str) -> &str {
            let cut = [DEMAND_SECTION, RETRY_SECTION, DISPOSITION_SECTION]
                .iter()
                .filter_map(|marker| rendered.find(marker))
                .min()
                .unwrap_or(rendered.len());
            &rendered[..cut]
        }
        let context = full_context();
        let advocate = render_for_role(AgentType::Advocate, &context);
        let adversary = render_for_role(AgentType::Adversary, &context);
        let judge = render_for_role(AgentType::Judge, &context);
        assert_eq!(shared_prefix(&advocate), shared_prefix(&adversary));
        assert_eq!(shared_prefix(&adversary), shared_prefix(&judge));
        assert!(
            shared_prefix(&judge).contains("Can the launcher share a cgroup across pods?"),
            "the shared body must actually carry the claim"
        );
        // The three renderings are genuinely different, or the assertion above
        // would hold trivially.
        assert_ne!(advocate, adversary);
        assert_ne!(adversary, judge);
        assert_ne!(advocate, judge);
    }

    /// AC3 — each role-scoped section is absent for the roles it is not for,
    /// asserted by the section marker.
    #[test]
    fn typed_evidence_context_role_scoping_removes_sections() {
        let context = full_context();
        let advocate = render_for_role(AgentType::Advocate, &context);
        let adversary = render_for_role(AgentType::Adversary, &context);
        let judge = render_for_role(AgentType::Judge, &context);

        assert!(
            !advocate.contains(DEMAND_SECTION),
            "Advocate sees no demand"
        );
        assert!(
            !advocate.contains(DISPOSITION_SECTION),
            "Advocate sees no disposition"
        );
        assert!(advocate.contains(RETRY_SECTION), "Advocate sees retry");

        assert!(
            !adversary.contains(RETRY_SECTION),
            "Adversary sees no retry"
        );
        assert!(
            !adversary.contains(DISPOSITION_SECTION),
            "Adversary sees no disposition"
        );
        assert!(adversary.contains(DEMAND_SECTION), "Adversary sees demand");

        assert!(judge.contains(DEMAND_SECTION), "Judge sees demand");
        assert!(judge.contains(RETRY_SECTION), "Judge sees retry");
        assert!(
            judge.contains(DISPOSITION_SECTION),
            "Judge sees disposition"
        );
    }

    /// The filters are what removes the sections — not missing data. Each
    /// predicate is checked directly, so widening one to `true` for every role
    /// turns the scoping assertions above red.
    #[test]
    fn typed_evidence_context_role_filters_are_exact() {
        for (role, demand, retry, disposition) in [
            (AgentType::Advocate, false, true, false),
            (AgentType::Adversary, true, false, false),
            (AgentType::Judge, true, true, true),
            (AgentType::Worker, false, false, false),
        ] {
            assert_eq!(shows_demand(role), demand, "demand for {role:?}");
            assert_eq!(shows_retry(role), retry, "retry for {role:?}");
            assert_eq!(
                shows_disposition(role),
                disposition,
                "disposition for {role:?}"
            );
        }
    }

    /// A control question renders the ordinary-objection guidance and lists no
    /// planned checks.
    #[test]
    fn typed_evidence_context_control_question_lists_no_checks() {
        let mut context = full_context();
        context.demand = Some(TypedEvidenceDemandContext {
            category: "preference".to_owned(),
            load_bearing: false,
            threshold: "A preference between two workable designs is not an evidence demand."
                .to_owned(),
            expected_checks: Vec::new(),
        });
        let rendered = render_for_role(AgentType::Adversary, &context);
        assert!(rendered.contains("Load-bearing: no"));
        assert!(rendered.contains("remains an ordinary objection"));
        assert!(
            !rendered.contains("Expected checks:\n-"),
            "a control must list no planned checks: {rendered}"
        );
    }
}
