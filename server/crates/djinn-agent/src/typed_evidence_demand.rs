//! Fixed demand-threshold guidance, one committed snapshot per case.
//!
//! An evidence demand parks a proposal and spends a spike. The threshold that
//! justifies one is therefore a contract, not a matter of taste, and these
//! snapshots are what pins it: four load-bearing categories that clear the bar
//! and list concrete expected checks, and two controls that do not and list
//! none.
//!
//! Every case renders through the same
//! [`djinn_roles::typed_evidence_context::render_for_role`] over the same
//! closed [`TypedEvidenceRoleContextV1`], so the guidance cannot grow a free-text
//! question inventory without failing the shared closed-shape assertion in
//! [`djinn_roles::typed_evidence_context::assert_closed_shape`].
//!
//! ## Scope
//!
//! Proposal `667e` AC 2 also requires proof that only authorized
//! Adversary/Judge paths create findings, that an identical delivery is
//! idempotent, that a distinct concurrent demand returns
//! `active_evidence_conflict`, and that the Advocate cannot resolve or
//! withdraw. None of those is provable by an agent-side snapshot — they are
//! repository and control-plane behaviours. They are covered by
//! `validate_demand_evidence`'s ordered checks in
//! `djinn-control-plane/src/tools/refinement_helpers.rs`, by
//! `djinn-db/tests/fixtures/typed_evidence_lifecycle_v1.json`, and by the
//! `bypass` rows of `typed_evidence_gate_matrix`. They are referenced here, not
//! duplicated.

#[cfg(test)]
mod tests {
    use djinn_roles::AgentType;
    use djinn_roles::typed_evidence_context::{
        TypedEvidenceContextLifecycle, TypedEvidenceDemandContext, TypedEvidenceRoleContextV1,
        assert_closed_shape, render_for_role,
    };

    /// One demand case: a category, its threshold, and the checks a spike is
    /// expected to run to settle it.
    struct DemandCase {
        name: &'static str,
        category: &'static str,
        load_bearing: bool,
        claim: &'static str,
        threshold: &'static str,
        expected_checks: &'static [&'static str],
    }

    /// The four load-bearing categories the epic names, plus two controls.
    ///
    /// A control is a question that a reasonable Adversary might raise and that
    /// must **not** become an evidence demand: a preference between two workable
    /// designs, and a question the repository already answers.
    const CASES: [DemandCase; 6] = [
        DemandCase {
            name: "feasibility",
            category: "feasibility",
            load_bearing: true,
            claim: "Can the launcher share a cgroup across pods?",
            threshold: "The spec cannot be accepted while it is unknown whether the kernel \
                        permits the delegation this design assumes.",
            expected_checks: &[
                "Run the cgroup delegation probe on a representative host and capture its exit status",
                "Read the launcher's cgroup attach path and record whether it can target a delegated subtree",
            ],
        },
        DemandCase {
            name: "safety",
            category: "safety",
            load_bearing: true,
            claim: "Can the compaction sweep delete a note that is still referenced?",
            threshold: "A destructive sweep may not ship while it is unknown whether it can \
                        remove data another surface still reads.",
            expected_checks: &[
                "Enumerate every reader of the notes table and record which ones the sweep does not consult",
                "Run the sweep against a fixture with a live inbound reference and record what it deletes",
            ],
        },
        DemandCase {
            name: "compatibility",
            category: "compatibility",
            load_bearing: true,
            claim: "Does the new launcher protocol parse a payload written by the previous release?",
            threshold: "A mixed-version fleet cannot be accepted while it is unknown whether \
                        the old writer's payload still decodes.",
            expected_checks: &[
                "Decode a payload captured from the previous release and record the result",
                "Read the protocol's version fence and record which versions it admits",
            ],
        },
        DemandCase {
            name: "rollout",
            category: "rollout",
            load_bearing: true,
            claim: "Does the settle window let a rolling deploy wedge admission?",
            threshold: "A rollout plan may not ship while it is unknown whether the deploy \
                        itself can stall the gate it depends on.",
            expected_checks: &[
                "Read the admission settle window and record its interaction with a surge rollout",
                "Run a simulated rolling deploy against the admission gate and record whether it admits",
            ],
        },
        // ── Controls ─────────────────────────────────────────────────────
        DemandCase {
            name: "control_preference",
            category: "preference",
            load_bearing: false,
            claim: "Should the retry budget be expressed in attempts or in seconds?",
            threshold: "A preference between two workable designs is not an evidence demand. \
                        Nothing about the spec is unknown; the choice is a judgement.",
            expected_checks: &[],
        },
        DemandCase {
            name: "control_repository_answerable",
            category: "repository_answerable",
            load_bearing: false,
            claim: "Which module owns the admission settle window today?",
            threshold: "A question the repository already answers is not an evidence demand. \
                        Read the code and file an ordinary objection if the answer is wrong.",
            expected_checks: &[],
        },
    ];

    fn context(case: &DemandCase) -> TypedEvidenceRoleContextV1 {
        let mut record = TypedEvidenceRoleContextV1::new(
            format!("01a01188-0000-7000-8000-0000000d{:04}", case.name.len()),
            TypedEvidenceContextLifecycle::Demanded,
            case.claim,
            2,
        );
        record.demand = Some(TypedEvidenceDemandContext {
            category: case.category.to_owned(),
            load_bearing: case.load_bearing,
            threshold: case.threshold.to_owned(),
            expected_checks: case
                .expected_checks
                .iter()
                .map(|check| (*check).to_owned())
                .collect(),
        });
        record
    }

    /// The Adversary is the role that raises demands, so the guidance is
    /// snapshotted as that role actually receives it.
    fn render(case: &DemandCase) -> String {
        render_for_role(AgentType::Adversary, &context(case))
    }

    /// AC1 — one committed snapshot per case: four load-bearing, two controls.
    #[test]
    fn typed_evidence_demand_guidance_snapshots() {
        assert_eq!(
            CASES.len(),
            6,
            "four load-bearing categories and two controls"
        );
        assert_eq!(
            CASES.iter().filter(|case| case.load_bearing).count(),
            4,
            "exactly four load-bearing categories"
        );
        for case in &CASES {
            insta::with_settings!({
                snapshot_path => "snapshots/typed_evidence_demand",
                snapshot_suffix => case.name,
                prepend_module_to_snapshot => false,
            }, {
                insta::assert_snapshot!("typed_evidence_demand", render(case));
            });
        }
    }

    /// AC2 — a load-bearing case names its category and lists concrete checks;
    /// a control renders the ordinary-objection guidance and lists none.
    #[test]
    fn typed_evidence_demand_load_bearing_and_controls_differ_exactly() {
        for case in &CASES {
            let rendered = render(case);
            assert!(
                rendered.contains(&format!("Category: {}", case.category)),
                "{}: the guidance must name its category: {rendered}",
                case.name
            );
            if case.load_bearing {
                assert!(
                    rendered.contains("Load-bearing: yes"),
                    "{}: must render as load-bearing",
                    case.name
                );
                assert!(
                    !rendered.contains("remains an ordinary objection"),
                    "{}: a load-bearing demand must not render the control guidance",
                    case.name
                );
                assert!(
                    !case.expected_checks.is_empty(),
                    "{}: a load-bearing case must declare checks",
                    case.name
                );
                for check in case.expected_checks {
                    assert!(
                        rendered.contains(check),
                        "{}: the guidance must list the concrete check {check:?}: {rendered}",
                        case.name
                    );
                }
            } else {
                assert!(
                    rendered.contains("Load-bearing: no"),
                    "{}: must render as not load-bearing",
                    case.name
                );
                assert!(
                    rendered.contains("remains an ordinary objection"),
                    "{}: a control must render the ordinary-objection guidance: {rendered}",
                    case.name
                );
                assert!(
                    !rendered.contains("Expected checks:\n-"),
                    "{}: a control must list no planned checks: {rendered}",
                    case.name
                );
            }
        }
        // The four load-bearing categories are distinct, so no snapshot is a
        // copy of another.
        let categories: std::collections::BTreeSet<&str> = CASES
            .iter()
            .filter(|case| case.load_bearing)
            .map(|case| case.category)
            .collect();
        assert_eq!(
            categories,
            ["compatibility", "feasibility", "rollout", "safety"]
                .into_iter()
                .collect(),
            "the four named load-bearing categories must each be covered once"
        );
    }

    /// AC3 — the demand guidance comes from the same closed field set as the
    /// role context, asserted through the one shared implementation.
    ///
    /// Because that record has no free-text question collection, a question
    /// inventory cannot be introduced through the demand path either.
    #[test]
    fn typed_evidence_demand_shares_the_closed_shape_contract() {
        for case in &CASES {
            assert_closed_shape(&context(case));
        }
    }

    /// The guidance is role-scoped like every other surface: an Advocate never
    /// sees the threshold it would otherwise be tempted to write to.
    #[test]
    fn typed_evidence_demand_guidance_is_adversary_and_judge_only() {
        let case = &CASES[0];
        let record = context(case);
        for role in [AgentType::Adversary, AgentType::Judge] {
            assert!(
                render_for_role(role, &record).contains("## Demand thresholds"),
                "{role:?} must receive the demand guidance"
            );
        }
        assert!(
            !render_for_role(AgentType::Advocate, &record).contains("## Demand thresholds"),
            "the Advocate must not receive the demand guidance"
        );
    }
}
