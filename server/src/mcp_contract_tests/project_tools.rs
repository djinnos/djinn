//! Remaining project-tool contract tests.
//!
//! All non-ignored tests migrated to `djinn-control-plane/tests/project_tools.rs`.
//! Only the `#[ignore]`d `project_remove_success_and_missing` regression
//! stays here — it tracks a Dolt multi-cascade DELETE bug unrelated to the
//! MCP tool surface itself; re-enable once Dolt can execute the cascade
//! without dropping the connection.  See `project_server_lib_test_flakes.md`.

use serde_json::json;

use crate::test_helpers::{initialize_mcp_session, mcp_call_tool};

// `project_remove` deletes the `projects` row, which fans out across ~8
// `ON DELETE CASCADE` FKs (epics, tasks, notes, sessions, agents,
// consolidation_metrics, verification_cache, task_runs).  `create_test_project_with_dir`
// also seeds 5 default agent rows.  Against the current test Dolt image
// (port 3307) the cascade fan-out drops the connection mid-query and sqlx
// surfaces `Io(UnexpectedEof)`.  This is the same Dolt limitation tracked on
// the sibling `djinn-db` test `delete_project` (server/crates/djinn-db/src/
// repositories/project.rs:649) and documented in the memory note
// `project_server_lib_test_flakes.md`.  Re-enable once Dolt can execute the
// multi-cascade DELETE without closing the conn.
#[ignore = "Dolt multi-cascade DELETE drops the connection; see project_server_lib_test_flakes.md"]
#[tokio::test]
async fn project_remove_success_and_missing() {
    // Create project directly in DB to bypass GitHub validation.
    let db = crate::test_helpers::create_test_db();
    let (project, _dir) = crate::test_helpers::create_test_project_with_dir(&db).await;
    let app = crate::test_helpers::create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;

    let removed = mcp_call_tool(
        &app,
        &session_id,
        "project_remove",
        json!({"project": project.slug()}),
    )
    .await;
    assert_eq!(removed["status"], "ok");

    let missing = mcp_call_tool(
        &app,
        &session_id,
        "project_remove",
        json!({"project": project.slug()}),
    )
    .await;
    assert!(
        missing["status"]
            .as_str()
            .unwrap_or_default()
            .starts_with("error:")
    );
}
