//! Sync GitHub OAuth tokens from the desktop token file to the server's credential vault.
//!
//! After login or token refresh, the desktop pushes the tokens to the server
//! so it can make GitHub API calls (PR creation, etc.) on behalf of the user.
//!
//! Communication uses the MCP JSON-RPC protocol over HTTP (`POST /mcp`).

use serde::{Deserialize, Serialize};

/// The credential key the server uses for GitHub App OAuth tokens.
const GITHUB_APP_OAUTH_DB_KEY: &str = "__OAUTH_GITHUB_APP";

/// Provider ID used when storing the credential.
const GITHUB_APP_PROVIDER_ID: &str = "github_app";

/// Token payload matching the server's `GitHubAppTokens` struct
/// (`server/crates/djinn-provider/src/oauth/github_app.rs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerGitHubAppTokens {
    access_token: String,
    refresh_token: String,
    /// Unix timestamp (seconds) when the access token expires.
    expires_at: i64,
    /// Unix timestamp when the refresh token expires (optional).
    refresh_token_expires_at: Option<i64>,
    /// GitHub user login.
    user_login: Option<String>,
}

/// Resolve the server base URL from `~/.djinn/active_connection.json`,
/// falling back to `~/.djinn/daemon.json` for backward compatibility.
///
/// This avoids coupling to Tauri managed state so the function can be called
/// from any async context (including background tasks that don't hold an
/// `AppHandle`).
fn resolve_server_base_url() -> Option<String> {
    // Prefer active_connection.json — written for all connection modes.
    if let Some(url) = crate::server::load_active_connection_url() {
        return Some(url);
    }

    // Fallback: daemon.json (local daemon mode only, backward compat).
    let home = dirs::home_dir()?;
    let daemon_path = home.join(".djinn").join("daemon.json");
    let content = std::fs::read_to_string(daemon_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    let port = json.get("port")?.as_u64()?;
    Some(format!("http://127.0.0.1:{}", port))
}

/// Push tokens to the server's credential vault via the MCP `credential_set` tool.
///
/// This is best-effort: failures are logged but never propagated to the caller
/// because the primary token storage (local file) has already succeeded.
pub async fn sync_tokens_to_server(
    access_token: &str,
    refresh_token: &str,
    expires_at: u64,
    user_login: Option<&str>,
) {
    let base_url = match resolve_server_base_url() {
        Some(u) => u,
        None => {
            log::warn!("token_sync: server URL not available, skipping credential sync");
            return;
        }
    };

    let server_tokens = ServerGitHubAppTokens {
        access_token: access_token.to_string(),
        refresh_token: refresh_token.to_string(),
        expires_at: expires_at as i64,
        refresh_token_expires_at: None,
        user_login: user_login.map(|s| s.to_string()),
    };

    let tokens_json = match serde_json::to_string(&server_tokens) {
        Ok(j) => j,
        Err(e) => {
            log::error!("token_sync: failed to serialize tokens: {}", e);
            return;
        }
    };

    if let Err(e) = do_sync(&base_url, &tokens_json).await {
        log::error!("token_sync: failed to sync tokens to server: {}", e);
    } else {
        log::info!("token_sync: successfully synced GitHub tokens to server");
    }
}

/// Perform the MCP handshake and call `credential_set`.
async fn do_sync(base_url: &str, tokens_json: &str) -> Result<(), String> {
    let client = reqwest::Client::new();
    let mcp_url = format!("{}/mcp", base_url);

    // Step 1: Initialize MCP session
    let init_payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {
                "name": "djinn-desktop-token-sync",
                "version": "0.1.0"
            }
        }
    });

    let init_resp = client
        .post(&mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&init_payload)
        .send()
        .await
        .map_err(|e| format!("MCP initialize request failed: {}", e))?;

    if !init_resp.status().is_success() {
        return Err(format!(
            "MCP initialize returned status {}",
            init_resp.status()
        ));
    }

    let session_id = init_resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "Missing mcp-session-id header on initialize response".to_string())?
        .to_string();

    // Consume the body so the connection is released.
    let _ = init_resp.text().await;

    // Step 2: Send notifications/initialized
    let notify_payload = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    });

    let notify_resp = client
        .post(&mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .json(&notify_payload)
        .send()
        .await
        .map_err(|e| format!("MCP initialized notification failed: {}", e))?;

    // Consume the body.
    let _ = notify_resp.text().await;

    // Step 3: Call credential_set tool
    let tool_payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "credential_set",
            "arguments": {
                "provider_id": GITHUB_APP_PROVIDER_ID,
                "key_name": GITHUB_APP_OAUTH_DB_KEY,
                "api_key": tokens_json,
            }
        }
    });

    let tool_resp = client
        .post(&mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .json(&tool_payload)
        .send()
        .await
        .map_err(|e| format!("credential_set request failed: {}", e))?;

    if !tool_resp.status().is_success() {
        return Err(format!(
            "credential_set returned status {}",
            tool_resp.status()
        ));
    }

    let body = tool_resp
        .text()
        .await
        .map_err(|e| format!("Failed to read credential_set response: {}", e))?;

    // Parse the response — may be plain JSON or SSE.
    let result = parse_jsonrpc_result(&body, 2)?;

    // Check for tool-level error in the result payload.
    if let Some(ok) = result.get("ok").and_then(|v| v.as_bool()) {
        if !ok {
            let err_msg = result
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            return Err(format!("credential_set returned ok=false: {}", err_msg));
        }
    }

    Ok(())
}

/// Parse a JSON-RPC 2.0 response that may be either a single JSON object or
/// an SSE stream containing multiple events.
fn parse_jsonrpc_result(body: &str, id: i64) -> Result<serde_json::Value, String> {
    // Try plain JSON first.
    if let Ok(single) = serde_json::from_str::<serde_json::Value>(body) {
        if single.get("id") == Some(&serde_json::Value::from(id)) {
            if let Some(error) = single.get("error") {
                let msg = error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown RPC error");
                return Err(format!("JSON-RPC error: {}", msg));
            }
            return extract_tool_payload(&single);
        }
    }

    // Parse as SSE stream.
    for line in body.lines() {
        let data = line.strip_prefix("data: ").unwrap_or(line);
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
            if event.get("id") == Some(&serde_json::Value::from(id)) {
                if let Some(error) = event.get("error") {
                    let msg = error
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown RPC error");
                    return Err(format!("JSON-RPC error: {}", msg));
                }
                return extract_tool_payload(&event);
            }
        }
    }

    Err(format!("No JSON-RPC response with id={} found in body", id))
}

/// Extract the tool result payload from a JSON-RPC result envelope.
fn extract_tool_payload(envelope: &serde_json::Value) -> Result<serde_json::Value, String> {
    let result = envelope
        .get("result")
        .ok_or_else(|| "Missing 'result' in JSON-RPC response".to_string())?;

    // Prefer structuredContent.
    if let Some(structured) = result.get("structuredContent") {
        return Ok(structured.clone());
    }

    // Fall back to parsing the first text content item.
    if let Some(content) = result.get("content").and_then(|v| v.as_array()) {
        for item in content {
            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
                    return Ok(parsed);
                }
            }
        }
    }

    Ok(result.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_tokens_serialization_matches_expected_format() {
        let tokens = ServerGitHubAppTokens {
            access_token: "ghu_abc123".to_string(),
            refresh_token: "ghr_def456".to_string(),
            expires_at: 1700000000,
            refresh_token_expires_at: Some(1715724800),
            user_login: Some("octocat".to_string()),
        };

        let json = serde_json::to_string(&tokens).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");

        assert_eq!(parsed["access_token"], "ghu_abc123");
        assert_eq!(parsed["refresh_token"], "ghr_def456");
        assert_eq!(parsed["expires_at"], 1700000000);
        assert_eq!(parsed["refresh_token_expires_at"], 1715724800);
        assert_eq!(parsed["user_login"], "octocat");
    }

    #[test]
    fn test_server_tokens_serialization_with_none_fields() {
        let tokens = ServerGitHubAppTokens {
            access_token: "ghu_abc".to_string(),
            refresh_token: "ghr_def".to_string(),
            expires_at: 1700000000,
            refresh_token_expires_at: None,
            user_login: None,
        };

        let json = serde_json::to_string(&tokens).expect("serialize");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");

        assert_eq!(parsed["access_token"], "ghu_abc");
        assert!(parsed["refresh_token_expires_at"].is_null());
        assert!(parsed["user_login"].is_null());
    }

    #[test]
    fn test_server_tokens_deserialization_from_server_format() {
        // Simulate what the server would produce
        let json = r#"{
            "access_token": "ghu_test",
            "refresh_token": "ghr_test",
            "expires_at": 1700000000,
            "refresh_token_expires_at": null,
            "user_login": "testuser"
        }"#;

        let tokens: ServerGitHubAppTokens = serde_json::from_str(json).expect("deserialize");
        assert_eq!(tokens.access_token, "ghu_test");
        assert_eq!(tokens.refresh_token, "ghr_test");
        assert_eq!(tokens.expires_at, 1700000000);
        assert!(tokens.refresh_token_expires_at.is_none());
        assert_eq!(tokens.user_login.as_deref(), Some("testuser"));
    }

    #[test]
    fn test_parse_jsonrpc_result_plain_json() {
        let body = r#"{"jsonrpc":"2.0","id":2,"result":{"structuredContent":{"ok":true,"success":true,"id":"abc","key_name":"__OAUTH_GITHUB_APP"}}}"#;
        let result = parse_jsonrpc_result(body, 2).expect("should parse");
        assert_eq!(result["ok"], true);
        assert_eq!(result["key_name"], "__OAUTH_GITHUB_APP");
    }

    #[test]
    fn test_parse_jsonrpc_result_error() {
        let body =
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"method not found"}}"#;
        let err = parse_jsonrpc_result(body, 2).unwrap_err();
        assert!(err.contains("method not found"));
    }

    #[test]
    fn test_parse_jsonrpc_result_sse_format() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\\"ok\\\":true,\\\"success\\\":true,\\\"id\\\":\\\"xyz\\\",\\\"key_name\\\":\\\"__OAUTH_GITHUB_APP\\\"}\"}]}}\n\n";
        let result = parse_jsonrpc_result(body, 2).expect("should parse SSE");
        assert_eq!(result["ok"], true);
    }
}
