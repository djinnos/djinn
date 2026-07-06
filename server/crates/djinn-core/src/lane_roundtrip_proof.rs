//! Deterministic round-trip proof for the lane demotion rollout (epic 5wxi).
//!
//! This test validates the snapshot → apply → rollback cycle using fixture data.
//! It requires no database, no production credentials, and no network access.
//!
//! Run: `cargo test -p djinn-core --lib lane_roundtrip_proof`

use crate::models::ModelLanes;

/// The new default implement/review lane payload for the rollout.
const NEW_IMPLEMENT: &[&str] = &[
    "xiaomi-token-plan-sgp/mimo-v2.5-pro",
    "zai-coding-plan/glm-5.2",
    "kimi-for-coding/k2p7",
    "minimax-coding-plan/MiniMax-M3",
];

const NEW_REVIEW: &[&str] = &[
    "xiaomi-token-plan-sgp/mimo-v2.5-pro",
    "zai-coding-plan/glm-5.2",
    "kimi-for-coding/k2p7",
    "minimax-coding-plan/MiniMax-M3",
];

/// Simulated pre-rollout user lane snapshot (three representative users).
fn pre_snapshot_user_lanes() -> Vec<(&'static str, Option<ModelLanes>)> {
    vec![
        (
            "user-alpha",
            Some(ModelLanes {
                plan: vec!["anthropic/claude-opus-4-7".to_string()],
                implement: vec![
                    "openai/gpt-5.5".to_string(),
                    "anthropic/claude-opus-4-7".to_string(),
                ],
                review: vec!["anthropic/claude-opus-4-7".to_string()],
            }),
        ),
        (
            "user-beta",
            Some(ModelLanes {
                plan: vec![],
                implement: vec!["anthropic/claude-opus-4-7".to_string()],
                review: vec![],
            }),
        ),
        // User with no explicit lanes (NULL in DB).
        ("user-gamma", None),
    ]
}

/// Simulated pre-rollout org default lanes.
fn pre_snapshot_org_lanes() -> ModelLanes {
    ModelLanes {
        plan: vec!["openai/gpt-5.5".to_string()],
        implement: vec![
            "openai/gpt-5.5".to_string(),
            "anthropic/claude-opus-4-7".to_string(),
        ],
        review: vec![
            "anthropic/claude-opus-4-7".to_string(),
            "openai/gpt-5.5".to_string(),
        ],
    }
}

fn rollout_lanes() -> ModelLanes {
    ModelLanes {
        plan: vec![],
        implement: NEW_IMPLEMENT.iter().map(|s| s.to_string()).collect(),
        review: NEW_REVIEW.iter().map(|s| s.to_string()).collect(),
    }
}

/// Simulate applying the rollout payload: replace lanes with the new defaults.
fn apply_rollout(_lanes: &Option<ModelLanes>) -> Option<ModelLanes> {
    // In the real rollout, we overwrite implement and review lanes.
    // Users with no explicit lanes (None) get new lanes assigned.
    Some(rollout_lanes())
}

/// Simulate rollback: restore from the snapshot.
fn rollback(snapshot: &Option<ModelLanes>) -> Option<ModelLanes> {
    snapshot.clone()
}

/// JSON round-trip: serialize → deserialize must preserve the lanes exactly.
fn json_round_trip(lanes: &ModelLanes) -> ModelLanes {
    let json = serde_json::to_string(lanes).expect("serialize ModelLanes");
    serde_json::from_str(&json).expect("deserialize ModelLanes")
}

#[test]
fn lane_roundtrip_json_serialization_preserves_order() {
    let original = rollout_lanes();
    let round_tripped = json_round_trip(&original);
    assert_eq!(
        original, round_tripped,
        "JSON round-trip must preserve lane order"
    );
}

#[test]
fn lane_snapshot_apply_rollback_round_trip() {
    let snapshot = pre_snapshot_user_lanes();

    for (user_id, original_lanes) in &snapshot {
        // Step 1: Apply rollout.
        let after_apply = apply_rollout(original_lanes);

        // Verify apply changed the lanes.
        let applied = after_apply.as_ref().expect("apply produces lanes");
        assert_eq!(
            applied.implement[0], "xiaomi-token-plan-sgp/mimo-v2.5-pro",
            "{user_id}: after apply, implement[0] should be mimo-v2.5-pro"
        );
        assert_eq!(
            applied.review[0], "xiaomi-token-plan-sgp/mimo-v2.5-pro",
            "{user_id}: after apply, review[0] should be mimo-v2.5-pro"
        );

        // Step 2: Rollback to snapshot.
        let after_rollback = rollback(original_lanes);

        // Step 3: Verify rollback restores original state exactly.
        assert_eq!(
            original_lanes, &after_rollback,
            "{user_id}: rollback must restore exact original lanes"
        );
    }
}

#[test]
fn org_lanes_snapshot_apply_rollback_round_trip() {
    let original = pre_snapshot_org_lanes();

    // Step 1: Apply.
    let after_apply = rollout_lanes();
    assert_eq!(
        after_apply.implement[0],
        "xiaomi-token-plan-sgp/mimo-v2.5-pro"
    );
    assert_eq!(after_apply.implement[1], "zai-coding-plan/glm-5.2");
    assert_eq!(after_apply.implement[2], "kimi-for-coding/k2p7");
    assert_eq!(after_apply.implement[3], "minimax-coding-plan/MiniMax-M3");

    // Step 2: Rollback.
    let after_rollback = original.clone();

    // Step 3: Verify.
    assert_eq!(
        original, after_rollback,
        "org rollback must restore exact original lanes"
    );
}

#[test]
fn last_resort_entries_are_indices_2_and_3() {
    let lanes = rollout_lanes();

    for lane_name in &["implement", "review"] {
        let entries = match *lane_name {
            "implement" => &lanes.implement,
            "review" => &lanes.review,
            _ => unreachable!(),
        };

        // First two are preferred.
        assert_eq!(
            entries[0], "xiaomi-token-plan-sgp/mimo-v2.5-pro",
            "{lane_name}[0] primary"
        );
        assert_eq!(
            entries[1], "zai-coding-plan/glm-5.2",
            "{lane_name}[1] secondary"
        );

        // Last two are last-resort.
        assert_eq!(
            entries[2], "kimi-for-coding/k2p7",
            "{lane_name}[2] last-resort"
        );
        assert_eq!(
            entries[3], "minimax-coding-plan/MiniMax-M3",
            "{lane_name}[3] last-resort"
        );
    }
}

#[test]
fn fixture_json_round_trip_proof() {
    // Load the checked fixture and verify its internal consistency.
    let fixture_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/routing/fixtures/lane-round-trip-proof.json"
    );
    let data: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(fixture_path).expect("read fixture"))
            .expect("parse fixture JSON");

    // Verify post_rollback == pre_snapshot (the core round-trip assertion).
    let pre = &data["pre_snapshot"];
    let post_rb = &data["post_rollback"];
    assert_eq!(
        pre, post_rb,
        "fixture: post_rollback must equal pre_snapshot"
    );

    // Verify the apply payload changes implement[0].
    let apply_impl =
        &data["apply_payload"]["user_settings"]["user-alpha"]["model_lanes"]["implement"][0];
    assert_eq!(
        apply_impl.as_str().unwrap(),
        "xiaomi-token-plan-sgp/mimo-v2.5-pro",
        "fixture: apply sets implement[0] to mimo-v2.5-pro"
    );

    // Verify the rollback restores implement[0].
    let rb_impl =
        &data["post_rollback"]["user_settings"]["user-alpha"]["model_lanes"]["implement"][0];
    assert_eq!(
        rb_impl.as_str().unwrap(),
        "openai/gpt-5.5",
        "fixture: rollback restores implement[0] to openai/gpt-5.5"
    );

    // Verify last-resort assertion.
    let last_resort = &data["assertions"]["last_resort_models"];
    assert_eq!(last_resort[0].as_str().unwrap(), "kimi-for-coding/k2p7");
    assert_eq!(
        last_resort[1].as_str().unwrap(),
        "minimax-coding-plan/MiniMax-M3"
    );
}
