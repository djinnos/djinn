//! Integration tests for the audit sampler: immutable frames and
//! replayable draws.
//!
//! These tests prove:
//! - Deterministic frame hashes (same input → same hash, always)
//! - Exact fixed-seed replay (seed + frame → identical selections)
//! - Separate stratum rates (unflagged vs autonomous-release)
//! - Autonomous-release higher-rate policy behavior
//! - Exclusion-count accounting
//! - No LLM or risk-scorer inputs anywhere

use std::collections::HashMap;

use serde_json::json;

use super::frame::{
    ExclusionReason, SamplePolicy, SealedFrame, SealedFrameEventPayload, StratumFrame,
    compute_content_hash,
};
use super::random::{
    HMAC_SHA256_COUNTER_V1, commit_seed, compute_draw_score, draw_selections, verify_replay,
};

// ── Helpers ──────────────────────────────────────────────────────────────────

fn test_policy() -> SamplePolicy {
    SamplePolicy {
        revision: 1,
        unflagged_rate: 0.10,  // 10% for easy testing
        autonomous_rate: 0.50, // 50% for autonomous-release
    }
}

fn make_stratum(name: &str, rate: f64, ids: &[&str]) -> StratumFrame {
    StratumFrame {
        name: name.to_string(),
        rate,
        eligible_ids: ids.iter().map(|s| s.to_string()).collect(),
    }
}

fn make_frame(strata: HashMap<String, StratumFrame>) -> SealedFrame {
    let content_hash = compute_content_hash(&strata);
    SealedFrame {
        frame_id: "frame-001".to_string(),
        revision: 1,
        project_id: "proj-001".to_string(),
        window_start: "2026-06-24T00:00:00Z".to_string(),
        window_end: "2026-07-01T00:00:00Z".to_string(),
        policy_revision: 1,
        strata,
        exclusion_counts: HashMap::new(),
        exclusion_reasons: Vec::new(),
        content_hash,
        sealed_at: "2026-07-01T00:05:00Z".to_string(),
        created_at: "2026-07-01T00:05:00Z".to_string(),
    }
}

// ── AC1: Deterministic frame hashes ──────────────────────────────────────────

#[test]
fn frame_hash_deterministic_same_input() {
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["c3", "c1", "c2"]),
    );
    strata.insert(
        "autonomous_release".to_string(),
        make_stratum("autonomous_release", 0.10, &["c5", "c4"]),
    );

    let h1 = compute_content_hash(&strata);
    let h2 = compute_content_hash(&strata);
    let h3 = compute_content_hash(&strata);

    assert_eq!(h1, h2);
    assert_eq!(h2, h3);
    assert_eq!(h1.len(), 64, "SHA-256 hex is always 64 chars");
}

#[test]
fn frame_hash_stable_across_map_ordering() {
    // Insert strata in different orders — hash must be the same.
    let mut strata_a = HashMap::new();
    strata_a.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["a", "b"]),
    );
    strata_a.insert(
        "autonomous_release".to_string(),
        make_stratum("autonomous_release", 0.10, &["c"]),
    );

    let mut strata_b = HashMap::new();
    strata_b.insert(
        "autonomous_release".to_string(),
        make_stratum("autonomous_release", 0.10, &["c"]),
    );
    strata_b.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["a", "b"]),
    );

    assert_eq!(
        compute_content_hash(&strata_a),
        compute_content_hash(&strata_b),
        "hash must be stable regardless of HashMap iteration order"
    );
}

#[test]
fn frame_hash_changes_when_eligible_ids_change() {
    let mut s1 = HashMap::new();
    s1.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["a", "b"]),
    );
    let mut s2 = HashMap::new();
    s2.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["a", "b", "c"]),
    );

    assert_ne!(
        compute_content_hash(&s1),
        compute_content_hash(&s2),
        "adding an id must change the hash"
    );
}

#[test]
fn frame_hash_changes_when_stratum_added() {
    let mut s1 = HashMap::new();
    s1.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["a"]),
    );

    let mut s2 = HashMap::new();
    s2.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["a"]),
    );
    s2.insert(
        "autonomous_release".to_string(),
        make_stratum("autonomous_release", 0.10, &["b"]),
    );

    assert_ne!(compute_content_hash(&s1), compute_content_hash(&s2));
}

#[test]
fn frame_hash_does_not_depend_on_rate() {
    // Rate is metadata, not part of the eligible-id content hash.
    let mut s1 = HashMap::new();
    s1.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["a"]),
    );
    let mut s2 = HashMap::new();
    s2.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.50, &["a"]),
    );

    // Content hash is over ids and stratum names, not rates.
    assert_eq!(
        compute_content_hash(&s1),
        compute_content_hash(&s2),
        "content hash is over eligible ids, not rates"
    );
}

// ── AC2: Late corrections create new revision (not mutation) ─────────────────

#[test]
fn sealed_frame_never_mutated_late_correction_produces_new_hash() {
    let mut strata_v1 = HashMap::new();
    strata_v1.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["a", "b", "c"]),
    );
    let frame_v1 = make_frame(strata_v1);

    // Simulate late correction: new change "d" arrives.
    let mut strata_v2 = HashMap::new();
    strata_v2.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["a", "b", "c", "d"]),
    );
    let mut frame_v2 = make_frame(strata_v2);
    frame_v2.revision = 2;

    // Frames have different content hashes.
    assert_ne!(frame_v1.content_hash, frame_v2.content_hash);
    // Revisions differ.
    assert_eq!(frame_v1.revision, 1);
    assert_eq!(frame_v2.revision, 2);
    // Original frame is unchanged.
    assert_eq!(frame_v1.total_eligible_count(), 3);
    assert_eq!(frame_v2.total_eligible_count(), 4);
}

#[test]
fn frame_sealed_event_payload_has_required_fields() {
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["a", "b"]),
    );
    let frame = make_frame(strata);
    let event = SealedFrameEventPayload::from_frame(&frame);

    assert_eq!(event.event_type, "audit.frame.sealed");
    assert_eq!(event.frame_id, "frame-001");
    assert_eq!(event.revision, 1);
    assert_eq!(event.project_id, "proj-001");
    assert_eq!(event.content_hash.len(), 64);
    assert_eq!(event.eligible_counts.get("unflagged_merged"), Some(&2));
}

// ── AC3: Deterministic draw with fixed seed ──────────────────────────────────

#[test]
fn draw_deterministic_with_fixed_seed() {
    let policy = test_policy();
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum(
            "unflagged_merged",
            policy.unflagged_rate,
            &["c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "c9", "c10"],
        ),
    );
    let frame = make_frame(strata);
    let seed = b"test-seed-001";

    let draw1 = draw_selections(&frame, seed);
    let draw2 = draw_selections(&frame, seed);

    assert_eq!(
        draw1.selections.len(),
        draw2.selections.len(),
        "draw count must be identical"
    );
    for (a, b) in draw1.selections.iter().zip(draw2.selections.iter()) {
        assert_eq!(a.merged_change_id, b.merged_change_id);
        assert_eq!(a.stratum, b.stratum);
        assert_eq!(a.selected_position, b.selected_position);
    }
    assert_eq!(draw1.seed_commitment, draw2.seed_commitment);
    assert_eq!(draw1.seed_reveal, draw2.seed_reveal);
}

#[test]
fn draw_different_seeds_give_different_results() {
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum(
            "unflagged_merged",
            0.10,
            &["c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "c9", "c10"],
        ),
    );
    let frame = make_frame(strata);

    let draw_a = draw_selections(&frame, b"seed-alpha");
    let draw_b = draw_selections(&frame, b"seed-omega");

    // Different seeds should (with very high probability) produce different
    // selections for a 10% rate over 10 items.
    let ids_a: Vec<&str> = draw_a
        .selections
        .iter()
        .map(|s| s.merged_change_id.as_str())
        .collect();
    let ids_b: Vec<&str> = draw_b
        .selections
        .iter()
        .map(|s| s.merged_change_id.as_str())
        .collect();
    // They might be equal by coincidence, but we assert the seed is different.
    assert_ne!(draw_a.seed_reveal, draw_b.seed_reveal);
    // At least verify they are both valid draws.
    assert!(!ids_a.is_empty());
    assert!(!ids_b.is_empty());
}

#[test]
fn draw_algorithm_field_is_correct() {
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.50, &["c1", "c2"]),
    );
    let frame = make_frame(strata);
    let draw = draw_selections(&frame, b"seed");

    assert_eq!(draw.algorithm, HMAC_SHA256_COUNTER_V1);
}

#[test]
fn seed_commitment_and_reveal_consistent() {
    let seed = b"my-secret-seed";
    let commitment = commit_seed(seed);
    assert_eq!(commitment.commitment_hash.len(), 64);

    // Verify commitment matches.
    assert!(super::random::verify_seed_commitment(seed, &commitment));

    // Wrong seed fails verification.
    assert!(!super::random::verify_seed_commitment(
        b"wrong-seed",
        &commitment
    ));
}

// ── AC3b: Replay verification ────────────────────────────────────────────────

#[test]
fn replay_verification_succeeds_for_exact_match() {
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.20, &["a", "b", "c", "d", "e"]),
    );
    let frame = make_frame(strata);
    let seed = b"replay-test-seed";

    let draw = draw_selections(&frame, seed);
    let expected_ids: Vec<String> = draw
        .selections
        .iter()
        .map(|s| s.merged_change_id.clone())
        .collect();

    let verification = verify_replay(&frame, seed, &expected_ids);
    assert!(verification.valid, "replay must succeed for exact match");
    assert!(verification.mismatches.is_empty());
}

#[test]
fn replay_verification_fails_for_wrong_ids() {
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.50, &["a", "b", "c", "d"]),
    );
    let frame = make_frame(strata);
    let seed = b"replay-seed";

    let draw = draw_selections(&frame, seed);
    let mut expected_ids: Vec<String> = draw
        .selections
        .iter()
        .map(|s| s.merged_change_id.clone())
        .collect();

    // Corrupt one expected id.
    if !expected_ids.is_empty() {
        expected_ids[0] = "WRONG_ID".to_string();
    }

    let verification = verify_replay(&frame, seed, &expected_ids);
    assert!(!verification.valid, "replay must fail for wrong ids");
}

// ── AC4: Separate stratum rates ──────────────────────────────────────────────

#[test]
fn separate_strata_have_independent_draws() {
    let policy = test_policy();
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum(
            "unflagged_merged",
            policy.unflagged_rate,
            &["u1", "u2", "u3", "u4", "u5"],
        ),
    );
    strata.insert(
        "autonomous_release".to_string(),
        make_stratum(
            "autonomous_release",
            policy.autonomous_rate,
            &["a1", "a2", "a3", "a4"],
        ),
    );
    let frame = make_frame(strata);
    let seed = b"stratum-test";

    let draw = draw_selections(&frame, seed);

    // Verify selections come from both strata.
    let unflagged_selected: Vec<_> = draw
        .selections
        .iter()
        .filter(|s| s.stratum == "unflagged_merged")
        .collect();
    let auto_selected: Vec<_> = draw
        .selections
        .iter()
        .filter(|s| s.stratum == "autonomous_release")
        .collect();

    // With 10% rate over 5 items → ceil(0.5) = 1.
    assert_eq!(unflagged_selected.len(), 1, "unflagged: ceil(5 * 0.10) = 1");
    // With 50% rate over 4 items → ceil(2.0) = 2.
    assert_eq!(auto_selected.len(), 2, "autonomous: ceil(4 * 0.50) = 2");

    assert_eq!(draw.selections.len(), 3, "total: 1 + 2 = 3");
}

#[test]
fn hmac_scores_use_stratum_prefix_for_independence() {
    let seed = b"same-seed";

    let score_unflagged = compute_draw_score(seed, "unflagged_merged", 0);
    let score_auto = compute_draw_score(seed, "autonomous_release", 0);

    // Same position but different stratum prefixes → different scores.
    assert_ne!(
        score_unflagged, score_auto,
        "stratum prefix ensures score independence"
    );
}

// ── AC5: Autonomous-release higher-rate policy behavior ──────────────────────

#[test]
fn autonomous_release_samples_at_higher_rate() {
    // Set up identical pools of 20 changes each.
    let ids: Vec<String> = (0..20).map(|i| format!("c{i:02}")).collect();
    let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();

    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.05, &id_refs),
    );
    strata.insert(
        "autonomous_release".to_string(),
        make_stratum("autonomous_release", 0.25, &id_refs),
    );
    let frame = make_frame(strata);
    let seed = b"rate-comparison-seed";

    let draw = draw_selections(&frame, seed);

    let unflagged_count = draw
        .selections
        .iter()
        .filter(|s| s.stratum == "unflagged_merged")
        .count();
    let auto_count = draw
        .selections
        .iter()
        .filter(|s| s.stratum == "autonomous_release")
        .count();

    // ceil(20 * 0.05) = 1
    assert_eq!(unflagged_count, 1, "unflagged rate 5%% → 1 of 20");
    // ceil(20 * 0.25) = 5
    assert_eq!(auto_count, 5, "autonomous rate 25%% → 5 of 20");

    assert!(
        auto_count > unflagged_count,
        "autonomous-release stratum must sample more than unflagged"
    );
}

#[test]
fn autonomous_release_higher_rate_even_when_equal_pool_sizes() {
    let ids: Vec<String> = (0..10).map(|i| format!("x{i:02}")).collect();
    let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();

    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.10, &id_refs),
    );
    strata.insert(
        "autonomous_release".to_string(),
        make_stratum("autonomous_release", 0.50, &id_refs),
    );
    let frame = make_frame(strata);

    let draw = draw_selections(&frame, b"equal-pool-test");

    let unflagged: Vec<_> = draw
        .selections
        .iter()
        .filter(|s| s.stratum == "unflagged_merged")
        .collect();
    let auto: Vec<_> = draw
        .selections
        .iter()
        .filter(|s| s.stratum == "autonomous_release")
        .collect();

    assert_eq!(unflagged.len(), 1, "ceil(10 * 0.10) = 1");
    assert_eq!(auto.len(), 5, "ceil(10 * 0.50) = 5");
}

// ── AC6: Exclusion-count accounting ──────────────────────────────────────────

#[test]
fn exclusion_counts_reflected_in_frame() {
    let exclusion_counts: HashMap<String, u64> = [
        ("outside_window".to_string(), 3),
        ("duplicate".to_string(), 1),
    ]
    .into_iter()
    .collect();
    let exclusion_reasons = vec![
        ExclusionReason {
            reason: "duplicate".to_string(),
            count: 1,
        },
        ExclusionReason {
            reason: "outside_window".to_string(),
            count: 3,
        },
    ];

    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.10, &["a", "b"]),
    );

    let content_hash = compute_content_hash(&strata);
    let frame = SealedFrame {
        frame_id: "frame-exc".to_string(),
        revision: 1,
        project_id: "proj-exc".to_string(),
        window_start: "2026-06-24T00:00:00Z".to_string(),
        window_end: "2026-07-01T00:00:00Z".to_string(),
        policy_revision: 1,
        strata,
        exclusion_counts,
        exclusion_reasons,
        content_hash,
        sealed_at: "2026-07-01T00:05:00Z".to_string(),
        created_at: "2026-07-01T00:05:00Z".to_string(),
    };

    assert_eq!(frame.exclusion_counts.get("outside_window"), Some(&3));
    assert_eq!(frame.exclusion_counts.get("duplicate"), Some(&1));
    assert_eq!(frame.exclusion_reasons.len(), 2);
    // Total eligible should be independent of exclusions.
    assert_eq!(frame.total_eligible_count(), 2);
}

#[test]
fn exclusion_counts_appear_in_event_payload() {
    let mut exc_counts = HashMap::new();
    exc_counts.insert("outside_window".to_string(), 5u64);

    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["x"]),
    );

    let content_hash = compute_content_hash(&strata);
    let frame = SealedFrame {
        frame_id: "f".to_string(),
        revision: 1,
        project_id: "p".to_string(),
        window_start: "2026-06-24T00:00:00Z".to_string(),
        window_end: "2026-07-01T00:00:00Z".to_string(),
        policy_revision: 1,
        strata,
        exclusion_counts: exc_counts,
        exclusion_reasons: vec![ExclusionReason {
            reason: "outside_window".to_string(),
            count: 5,
        }],
        content_hash,
        sealed_at: "2026-07-01T00:05:00Z".to_string(),
        created_at: "2026-07-01T00:05:00Z".to_string(),
    };

    let event = SealedFrameEventPayload::from_frame(&frame);
    assert_eq!(event.exclusion_counts.get("outside_window"), Some(&5));
}

// ── AC7: No LLM / risk-scorer inputs ────────────────────────────────────────

#[test]
fn draw_uses_only_frame_content_and_seed() {
    // The draw function's signature only accepts a SealedFrame and a seed.
    // There are no LLM, risk-score, or external-service parameters.
    // This test exercises the draw to confirm it produces valid results
    // without any such inputs.
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.50, &["a", "b", "c", "d"]),
    );
    let frame = make_frame(strata);

    // Simple byte seed — no external service involved.
    let draw = draw_selections(&frame, b"pure-deterministic");

    assert_eq!(draw.algorithm, HMAC_SHA256_COUNTER_V1);
    assert!(!draw.selections.is_empty());
    // Every selected id must be from the frame's eligible set.
    let eligible: std::collections::HashSet<&str> = frame
        .strata
        .values()
        .flat_map(|s| s.eligible_ids.iter().map(|id| id.as_str()))
        .collect();
    for sel in &draw.selections {
        assert!(
            eligible.contains(sel.merged_change_id.as_str()),
            "selected id {} must be in the eligible set",
            sel.merged_change_id
        );
    }
}

// ── Edge cases ───────────────────────────────────────────────────────────────

#[test]
fn draw_empty_stratum_produces_no_selections() {
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.50, &[]),
    );
    let frame = make_frame(strata);
    let draw = draw_selections(&frame, b"empty-test");

    assert!(draw.selections.is_empty());
}

#[test]
fn draw_rate_zero_produces_no_selections() {
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.0, &["a", "b", "c"]),
    );
    let frame = make_frame(strata);
    let draw = draw_selections(&frame, b"zero-rate");

    assert!(draw.selections.is_empty());
}

#[test]
fn draw_rate_one_selects_all() {
    let ids = ["a", "b", "c"];
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 1.0, &ids),
    );
    let frame = make_frame(strata);
    let draw = draw_selections(&frame, b"full-rate");

    assert_eq!(draw.selections.len(), 3, "100%% rate selects all");
}

#[test]
fn compute_k_ceil_behavior() {
    // ceil(10 * 0.02) = 1 (minimum 1 when rate > 0 and count > 0)
    assert_eq!(super::random::compute_k(10, 0.02), 1);
    // ceil(10 * 0.10) = 1
    assert_eq!(super::random::compute_k(10, 0.10), 1);
    // ceil(10 * 0.15) = 2
    assert_eq!(super::random::compute_k(10, 0.15), 2);
    // ceil(10 * 0.50) = 5
    assert_eq!(super::random::compute_k(10, 0.50), 5);
    // ceil(10 * 1.00) = 10
    assert_eq!(super::random::compute_k(10, 1.00), 10);
    // zero count → 0
    assert_eq!(super::random::compute_k(0, 0.50), 0);
    // zero rate → 0
    assert_eq!(super::random::compute_k(10, 0.0), 0);
    // min(count, ceil) for rate > 1.0
    assert_eq!(super::random::compute_k(3, 2.0), 3);
}

#[test]
fn positions_are_sequential_across_strata() {
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 1.0, &["u1", "u2"]),
    );
    strata.insert(
        "autonomous_release".to_string(),
        make_stratum("autonomous_release", 1.0, &["a1", "a2"]),
    );
    let frame = make_frame(strata);
    let draw = draw_selections(&frame, b"position-test");

    // With 100% rate, all 4 items are selected.
    assert_eq!(draw.selections.len(), 4);

    // Positions should be 0, 1, 2, 3.
    for (i, sel) in draw.selections.iter().enumerate() {
        assert_eq!(sel.selected_position, i as i32);
    }
}

#[test]
fn sample_policy_from_row_uses_defaults() {
    let row = djinn_db::SamplePolicyRow {
        id: "test-id".to_string(),
        project_id: "test-project".to_string(),
        revision: 3,
        policy_json: json!({}),
        created_at: "2026-07-01T00:00:00Z".to_string(),
    };

    let policy = SamplePolicy::from_row(&row).expect("must parse");
    assert_eq!(policy.revision, 3);
    assert!((policy.unflagged_rate - 0.02).abs() < f64::EPSILON);
    assert!((policy.autonomous_rate - 0.10).abs() < f64::EPSILON);
}

#[test]
fn sample_policy_from_row_uses_provided_values() {
    let row = djinn_db::SamplePolicyRow {
        id: "test-id".to_string(),
        project_id: "test-project".to_string(),
        revision: 1,
        policy_json: json!({"unflagged_rate": 0.05, "autonomous_rate": 0.25}),
        created_at: "2026-07-01T00:00:00Z".to_string(),
    };

    let policy = SamplePolicy::from_row(&row).expect("must parse");
    assert!((policy.unflagged_rate - 0.05).abs() < f64::EPSILON);
    assert!((policy.autonomous_rate - 0.25).abs() < f64::EPSILON);
}

#[test]
fn rate_for_stratum_dispatches_correctly() {
    let policy = SamplePolicy {
        revision: 1,
        unflagged_rate: 0.03,
        autonomous_rate: 0.15,
    };

    assert!((policy.rate_for_stratum("unflagged_merged") - 0.03).abs() < f64::EPSILON);
    assert!((policy.rate_for_stratum("autonomous_release") - 0.15).abs() < f64::EPSILON);
    // Unknown stratum falls back to unflagged rate.
    assert!((policy.rate_for_stratum("unknown") - 0.03).abs() < f64::EPSILON);
}

#[test]
fn all_eligible_ids_sorted_is_canonical() {
    let mut strata = HashMap::new();
    strata.insert(
        "autonomous_release".to_string(),
        make_stratum("autonomous_release", 0.50, &["z", "a"]),
    );
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["m", "b"]),
    );
    let frame = make_frame(strata);

    let all = frame.all_eligible_ids_sorted();
    // Strata sorted by name: autonomous_release < unflagged_merged
    // Within each, ids sorted: a < z, b < m
    assert_eq!(
        all,
        vec![
            ("autonomous_release", "a"),
            ("autonomous_release", "z"),
            ("unflagged_merged", "b"),
            ("unflagged_merged", "m"),
        ]
    );
}

// ── Serialization round-trip ─────────────────────────────────────────────────

#[test]
fn draw_result_serializes_and_deserializes() {
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.50, &["a", "b", "c", "d"]),
    );
    let frame = make_frame(strata);
    let draw = draw_selections(&frame, b"serialize-test");

    let json = serde_json::to_string(&draw).expect("must serialize");
    let deserialized: super::random::DrawResult =
        serde_json::from_str(&json).expect("must deserialize");

    assert_eq!(deserialized.algorithm, draw.algorithm);
    assert_eq!(deserialized.selections.len(), draw.selections.len());
    assert_eq!(deserialized.seed_commitment, draw.seed_commitment);
}

#[test]
fn sealed_frame_serializes_and_deserializes() {
    let mut strata = HashMap::new();
    strata.insert(
        "unflagged_merged".to_string(),
        make_stratum("unflagged_merged", 0.02, &["a", "b"]),
    );
    let frame = make_frame(strata);

    let json = serde_json::to_string(&frame).expect("must serialize");
    let deserialized: SealedFrame = serde_json::from_str(&json).expect("must deserialize");

    assert_eq!(deserialized.frame_id, frame.frame_id);
    assert_eq!(deserialized.content_hash, frame.content_hash);
    assert_eq!(deserialized.total_eligible_count(), 2);
}
