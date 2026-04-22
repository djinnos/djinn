use std::path::Path;

use djinn_core::models::DjinnSettings;
use djinn_provider::repos::CredentialRepository;
use tokio_util::sync::CancellationToken;

use crate::server::AppState;
use crate::test_helpers;

#[test]
fn mcp_tools_do_not_use_untyped_json_output() {
    // Bare serde_json::Value generates `true` as its JSON Schema, which
    // strict MCP clients (e.g. Claude Code) reject — breaking the entire
    // tool list.  Use AnyJson or ObjectJson wrappers instead.
    const FORBIDDEN: &[&str] = &[
        "Json<serde_json::Value>",
        "Vec<serde_json::Value>",
        "Option<serde_json::Value>",
        "Option<Vec<serde_json::Value>>",
    ];

    fn visit(dir: &Path, offenders: &mut Vec<String>) {
        let entries = std::fs::read_dir(dir).expect("read tools directory");
        for entry in entries {
            let entry = entry.expect("read entry");
            let path = entry.path();
            if path.is_dir() {
                visit(&path, offenders);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // Skip the json_object.rs helper (it wraps Value on purpose).
            if path
                .file_name()
                .map(|n| n == "json_object.rs")
                .unwrap_or(false)
            {
                continue;
            }
            let content = std::fs::read_to_string(&path).expect("read rust file");
            for pat in FORBIDDEN {
                if content.contains(pat) {
                    offenders.push(format!("{}  (contains `{}`)", path.display(), pat));
                }
            }
        }
    }

    let tools_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/djinn-mcp/src/tools");
    let mut offenders = Vec::new();
    visit(&tools_dir, &mut offenders);

    assert!(
        offenders.is_empty(),
        "Found bare serde_json::Value in MCP tool structs (use AnyJson/ObjectJson instead):\n  {}",
        offenders.join("\n  ")
    );
}

/// Unit test: verify the in-memory test DB has migrations applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_db_has_tables() {
    let db = test_helpers::create_test_db();
    db.ensure_initialized().await.unwrap();

    let exists = db.table_exists("settings").await.unwrap();
    assert!(exists, "settings table should exist");
}

/// Demonstrates tokio::test(start_paused = true) for time-dependent logic.
/// With start_paused, tokio::time::sleep completes instantly (time is virtual).
#[tokio::test(start_paused = true)]
async fn time_paused_pattern() {
    let before = tokio::time::Instant::now();
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    let elapsed = before.elapsed();

    // With start_paused, the 60s sleep advances virtual time instantly.
    assert_eq!(elapsed.as_secs(), 60);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_settings_rejects_disconnected_model_priority_provider() {
    let db = test_helpers::create_test_db();
    let state = AppState::new(db, CancellationToken::new());

    let settings = DjinnSettings {
        models: Some(vec!["nvidia/moonshotai/kimi-k2-instruct".into()]),
        ..Default::default()
    };

    let err = state
        .apply_settings(&settings)
        .await
        .expect_err("should reject disconnected provider");

    assert!(err.contains("disconnected providers"));
    assert!(err.contains("nvidia"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_settings_accepts_connected_model_priority_provider() {
    let db = test_helpers::create_test_db();
    let state = AppState::new(db, CancellationToken::new());

    let cred_repo = CredentialRepository::new(state.db().clone(), state.event_bus());
    cred_repo
        .set("synthetic", "SYNTHETIC_API_KEY", "sk-test")
        .await
        .unwrap();

    let settings = DjinnSettings {
        models: Some(vec!["synthetic/hf:moonshotai/Kimi-K2.5".into()]),
        ..Default::default()
    };

    state
        .apply_settings(&settings)
        .await
        .expect("connected provider should be accepted");
}
