//! Taxonomy-v1 retrieval contract tests.

use super::*;

fn ts(hour: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_704_000_000).unwrap() + time::Duration::hours(hour)
}

fn v1_group(
    project: &str,
    entry: &str,
    successful: u64,
    zero: u64,
    candidate_bearing: u64,
    starved: u64,
) -> TaxonomyV1ValidGroupSnapshot {
    TaxonomyV1ValidGroupSnapshot {
        project_id: project.into(),
        entry_point: entry.into(),
        taxonomy_version: RETRIEVAL_TAXONOMY_V1.into(),
        window_start: ts(0),
        window_end: ts(24),
        refreshed_at: ts(25),
        counters: TaxonomyV1QueryCounters {
            total_queries: successful + 3,
            successful_queries: successful,
            errored_queries: 2,
            cancelled_queries: 1,
            zero_candidate_queries: zero,
            candidate_bearing_queries: candidate_bearing,
            starved_queries: starved,
            injected_queries: candidate_bearing - starved,
        },
        candidate_total: 10,
        injected_total: 2,
        dispositions: TaxonomyV1DispositionHistogram {
            confidence_filtered: 1,
            not_top_k: 2,
            oversized_skipped: 3,
            injected: 2,
            budget_pruned: 2,
        },
        legacy_unclassified_queries: 5,
        invalid_taxonomy_queries: 0,
    }
}

fn v1_snapshot(
    groups: impl IntoIterator<Item = TaxonomyV1ValidGroupSnapshot>,
) -> TaxonomyV1RetrievalSnapshot {
    let mut snapshot = TaxonomyV1RetrievalSnapshot::new();
    for group in groups {
        snapshot.insert_valid_group(group);
    }
    snapshot
}

fn v1_config() -> KnowledgeInjectionConfig {
    KnowledgeInjectionConfig {
        injection_starvation_threshold_percent: 50,
        injection_starvation_query_floor: 2,
        retrieval_health_window_minutes: 60,
        ..KnowledgeInjectionConfig::default()
    }
}

#[test]
fn v1_resolvers_are_inclusive_independent_and_exclude_terminal_failures() {
    let mut error_heavy = v1_group("p1", "memory_search", 2, 1, 0, 0);
    error_heavy.counters.total_queries = 102;
    error_heavy.counters.errored_queries = 99;
    let snapshot = v1_snapshot([
        error_heavy,
        v1_group("p2", "memory_search", 2, 0, 0, 0),
        v1_group("p1", LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT, 2, 0, 2, 1),
    ]);
    let zero = resolve_retrieval_zero_result_v1(&snapshot, v1_config());
    assert_eq!(zero.len(), 1, "50 percent equality triggers");
    assert_eq!(
        zero[0].entity_ids["finding_key"],
        "memory.retrieval_zero_result:p1:memory_search"
    );
    assert_eq!(
        zero[0].evidence["exact_ratio"], "1/2",
        "errors are excluded from the denominator"
    );
    let starvation = resolve_injection_starvation_v1(&snapshot, v1_config());
    assert_eq!(starvation.len(), 1);
    assert_eq!(
        starvation[0].entity_ids["finding_key"],
        "memory.injection_starvation:p1:load_knowledge_context"
    );
}

#[test]
fn v1_floor_zero_candidates_and_healthy_groups_do_not_emit() {
    let snapshot = v1_snapshot([
        v1_group("below-floor", "memory_search", 1, 1, 0, 0),
        v1_group(
            "zero-candidates",
            LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT,
            2,
            0,
            0,
            0,
        ),
        v1_group("healthy", "memory_search", 2, 0, 2, 0),
    ]);
    assert!(resolve_retrieval_zero_result_v1(&snapshot, v1_config()).is_empty());
    assert!(resolve_injection_starvation_v1(&snapshot, v1_config()).is_empty());
}

#[test]
fn v1_historical_starvation_shape_only_evaluates_load_knowledge_context() {
    // The 2026-07-14 incident shape: candidate-bearing successful loads,
    // but every candidate was skipped by budget packing.
    let mut historical = v1_group("history", LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT, 20, 0, 20, 20);
    historical.candidate_total = 20;
    historical.injected_total = 0;
    historical.dispositions = TaxonomyV1DispositionHistogram {
        confidence_filtered: 0,
        not_top_k: 0,
        oversized_skipped: 0,
        injected: 0,
        budget_pruned: 20,
    };
    let snapshot = v1_snapshot([
        historical,
        v1_group("history", "memory_search", 20, 0, 20, 20),
    ]);
    let findings = resolve_injection_starvation_v1(&snapshot, v1_config());
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].evidence["exact_ratio"], "20/20");
    assert_eq!(
        findings[0].entity_ids["entry_point"],
        LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT
    );
}

#[test]
fn v1_snapshot_groups_are_mutually_exclusive_and_invalid_groups_are_not_evaluated() {
    let mut snapshot = v1_snapshot([v1_group(
        "project",
        LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT,
        2,
        0,
        2,
        1,
    )]);
    let replaced = snapshot.insert_valid_group(v1_group(
        "project",
        LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT,
        2,
        0,
        2,
        1,
    ));
    assert!(replaced.is_some(), "the group key is unique");
    assert_eq!(
        resolve_injection_starvation_v1(&snapshot, v1_config()).len(),
        1
    );
    let invalid = TaxonomyV1InvalidGroupSnapshot {
        project_id: "project".into(),
        entry_point: LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT.into(),
        taxonomy_version: RETRIEVAL_TAXONOMY_V1.into(),
        window_start: ts(0),
        window_end: ts(24),
        refreshed_at: ts(25),
        legacy_unclassified_queries: 4,
        invalid_taxonomy_queries: 3,
        invalid_reason: "malformed disposition".into(),
    };
    assert_eq!(
        invalid.finding_key(RETRIEVAL_ZERO_RESULT_NAME),
        "memory.retrieval_zero_result:project:load_knowledge_context"
    );
    assert_eq!(
        invalid.finding_key(INJECTION_STARVATION_NAME),
        "memory.injection_starvation:project:load_knowledge_context"
    );
    snapshot.insert_invalid_group(invalid);
    assert_eq!(snapshot.invalid_groups().count(), 1);
    assert_eq!(snapshot.valid_groups().count(), 0);
    assert!(resolve_injection_starvation_v1(&snapshot, v1_config()).is_empty());

    snapshot.insert_valid_group(v1_group(
        "project",
        LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT,
        2,
        0,
        2,
        1,
    ));
    assert_eq!(
        snapshot.invalid_groups().count(),
        0,
        "a valid replacement removes invalid state"
    );
    assert_eq!(
        resolve_injection_starvation_v1(&snapshot, v1_config()).len(),
        1
    );
}

#[test]
fn v1_evidence_has_complete_candidate_taxonomy_and_wrappers_are_cheap() {
    let snapshot = v1_snapshot([v1_group("p", "memory_search", 2, 2, 0, 0)]);
    let finding = resolve_retrieval_zero_result_v1(&snapshot, v1_config())
        .pop()
        .unwrap();
    let evidence = &finding.evidence;
    for field in [
        "project_id",
        "entry_point",
        "taxonomy_version",
        "window",
        "refreshed_at",
        "numerator",
        "denominator",
        "exact_ratio",
        "configured_threshold_percent",
        "configured_query_floor",
        "configured_window_minutes",
        "query_counters",
        "candidate_total",
        "injected_total",
        "dispositions",
        "legacy_unclassified_queries",
        "invalid_taxonomy_queries",
    ] {
        assert!(!evidence[field].is_null(), "missing {field}");
    }
    for field in [
        "confidence_filtered",
        "not_top_k",
        "oversized_skipped",
        "injected",
        "budget_pruned",
    ] {
        assert!(
            !evidence["dispositions"][field].is_null(),
            "missing disposition {field}"
        );
    }
    assert_eq!(evidence["query_counters"]["cancelled_queries"], 1);
    assert_eq!(evidence["dispositions"]["budget_pruned"], 2);
    assert_eq!(
        TaxonomyV1RetrievalZeroResultCheck::new(v1_config(), snapshot.clone()).cadence(),
        DoctorCheckCadence::Cheap
    );
    assert_eq!(
        InjectionStarvationCheck::new(v1_config(), snapshot).cadence(),
        DoctorCheckCadence::Cheap
    );
}

#[test]
fn v1_public_facade_constructs_valid_and_invalid_adapter_shapes() {
    use crate::doctor::{
        InjectionStarvationCheck as PublicInjectionStarvationCheck,
        TaxonomyV1DispositionHistogram as PublicDispositionHistogram,
        TaxonomyV1InvalidGroupSnapshot as PublicInvalidGroup,
        TaxonomyV1QueryCounters as PublicQueryCounters,
        TaxonomyV1RetrievalSnapshot as PublicSnapshot,
        TaxonomyV1RetrievalZeroResultCheck as PublicZeroResultCheck,
        TaxonomyV1ValidGroupSnapshot as PublicValidGroup,
        resolve_injection_starvation_v1 as public_resolve_starvation,
        resolve_retrieval_zero_result_v1 as public_resolve_zero_result,
    };

    let valid = PublicValidGroup {
        project_id: "healthy-project".into(),
        entry_point: "memory_search".into(),
        taxonomy_version: RETRIEVAL_TAXONOMY_V1.into(),
        window_start: ts(0),
        window_end: ts(60),
        refreshed_at: ts(61),
        counters: PublicQueryCounters {
            total_queries: 4,
            successful_queries: 2,
            errored_queries: 1,
            cancelled_queries: 1,
            zero_candidate_queries: 1,
            candidate_bearing_queries: 1,
            starved_queries: 0,
            injected_queries: 1,
        },
        candidate_total: 5,
        injected_total: 1,
        dispositions: PublicDispositionHistogram {
            confidence_filtered: 1,
            not_top_k: 1,
            oversized_skipped: 1,
            injected: 1,
            budget_pruned: 1,
        },
        legacy_unclassified_queries: 2,
        invalid_taxonomy_queries: 0,
    };
    let invalid = PublicInvalidGroup {
        project_id: "malformed-project".into(),
        entry_point: LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT.into(),
        taxonomy_version: RETRIEVAL_TAXONOMY_V1.into(),
        window_start: ts(0),
        window_end: ts(60),
        refreshed_at: ts(61),
        legacy_unclassified_queries: 3,
        invalid_taxonomy_queries: 1,
        invalid_reason: "candidate histogram does not match total".into(),
    };
    let valid_key = valid.group_key();
    let invalid_key = invalid.group_key();
    let snapshot = PublicSnapshot::from_groups([valid], [invalid]);

    assert_eq!(
        valid_key,
        TaxonomyV1GroupKey::new("healthy-project", "memory_search")
    );
    assert_eq!(
        invalid_key,
        TaxonomyV1GroupKey::new("malformed-project", LOAD_KNOWLEDGE_CONTEXT_ENTRY_POINT)
    );
    assert_eq!(snapshot.valid_groups().count(), 1);
    assert_eq!(snapshot.invalid_groups().count(), 1);
    assert_eq!(public_resolve_zero_result(&snapshot, v1_config()).len(), 1);
    assert!(public_resolve_starvation(&snapshot, v1_config()).is_empty());
    assert_eq!(
        PublicZeroResultCheck::new(v1_config(), snapshot.clone()).cadence(),
        DoctorCheckCadence::Cheap
    );
    assert_eq!(
        PublicInjectionStarvationCheck::new(v1_config(), snapshot).cadence(),
        DoctorCheckCadence::Cheap
    );
}
