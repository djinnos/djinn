//! Contract tests for per-user settings that do not require live providers.

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::auth_context::SESSION_USER_ID;
use djinn_db::UserSettingsRepository;
use djinn_db::repositories::user::UserRepository;
use serde_json::json;

async fn signed_in_user(harness: &McpTestHarness, suffix: &str) -> String {
    UserRepository::new(harness.db().clone())
        .upsert_from_github(
            suffix.bytes().map(i64::from).sum::<i64>() + 9_000_000,
            &format!("lane-limits-{suffix}"),
            None,
            None,
        )
        .await
        .expect("create test user")
        .id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_max_sessions_is_unset_by_default_and_round_trips() {
    let harness = McpTestHarness::new().await;
    let user_id = signed_in_user(&harness, "round-trip").await;

    let before = SESSION_USER_ID
        .scope(Some(user_id.clone()), async {
            harness.call_tool("user_settings_get", json!({})).await
        })
        .await
        .expect("get default user settings");
    assert!(before["ok"].as_bool().unwrap_or(false));
    assert_eq!(before["lane_max_sessions"], serde_json::Value::Null);

    let set = SESSION_USER_ID
        .scope(Some(user_id.clone()), async {
            harness
                .call_tool(
                    "user_settings_set",
                    json!({
                        "lane_max_sessions": {
                            "plan": 1,
                            "implement": 3,
                            "review": 2
                        }
                    }),
                )
                .await
        })
        .await
        .expect("set lane max sessions");
    assert!(set["ok"].as_bool().unwrap_or(false), "response: {set}");
    assert!(set["applied"].as_bool().unwrap_or(false));
    assert_eq!(
        set["lane_max_sessions"],
        json!({"plan": 1, "implement": 3, "review": 2})
    );

    let get = SESSION_USER_ID
        .scope(Some(user_id), async {
            harness.call_tool("user_settings_get", json!({})).await
        })
        .await
        .expect("read lane max sessions");
    assert_eq!(
        get["lane_max_sessions"],
        json!({"plan": 1, "implement": 3, "review": 2})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_max_sessions_rejects_values_outside_one_through_ten() {
    let harness = McpTestHarness::new().await;
    let user_id = signed_in_user(&harness, "validation").await;

    for (payload, expected_lane) in [
        (json!({"plan": 0, "implement": 3, "review": 1}), "plan"),
        (
            json!({"plan": 1, "implement": 11, "review": 1}),
            "implement",
        ),
    ] {
        let rejected = SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                harness
                    .call_tool("user_settings_set", json!({"lane_max_sessions": payload}))
                    .await
            })
            .await
            .expect("dispatch invalid lane max sessions");
        assert!(!rejected["ok"].as_bool().unwrap_or(true));
        assert_eq!(rejected["applied"], false);
        assert!(
            rejected["error"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!("lane_max_sessions.{expected_lane}")),
            "response: {rejected}"
        );
    }

    let persisted = UserSettingsRepository::new(harness.db().clone())
        .get_or_default(&user_id)
        .await
        .expect("read settings after rejected patches");
    assert!(persisted.lane_max_sessions.is_none());
}
