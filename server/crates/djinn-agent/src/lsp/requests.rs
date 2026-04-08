use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use serde_json::json;

use super::client::{
    ClientStdin, OpenedFiles, PendingResponses, clone_client_request_refs, ensure_did_open,
    send_request, spawn_client,
};
use super::server_config::server_for_path;
use super::symbols::{SymbolQuery, format_document_symbols, resolve_symbol_position};
use super::workspace::find_root;
use super::{LspManager, REQUEST_TIMEOUT};

pub(super) async fn fetch_document_symbols(
    manager: &LspManager,
    worktree: &Path,
    path: &Path,
) -> Result<Vec<serde_json::Value>, String> {
    let (stdin, pending, seq, opened) = get_request_refs(manager, worktree, path).await?;
    let uri = ensure_did_open(&stdin, path, &opened).await?;

    let result = send_request(
        &stdin,
        &pending,
        &seq,
        "textDocument/documentSymbol",
        json!({
            "textDocument": { "uri": uri },
        }),
        REQUEST_TIMEOUT,
    )
    .await?;

    if result.is_null() {
        return Ok(Vec::new());
    }

    let symbols = result.as_array().cloned().unwrap_or_default();
    if symbols.is_empty() {
        return Ok(Vec::new());
    }

    Ok(symbols)
}

pub(super) async fn resolve_symbol_to_position(
    manager: &LspManager,
    worktree: &Path,
    path: &Path,
    symbol_query: &str,
) -> Result<(u32, u32), String> {
    let symbols = fetch_document_symbols(manager, worktree, path).await?;
    let position = resolve_symbol_position(&symbols, symbol_query)?;
    Ok((position.line, position.character))
}

pub(super) async fn get_request_refs(
    manager: &LspManager,
    worktree: &Path,
    path: &Path,
) -> Result<(ClientStdin, PendingResponses, Arc<AtomicU64>, OpenedFiles), String> {
    let server = server_for_path(path)
        .ok_or_else(|| format!("no LSP server configured for {}", path.display()))?;
    let root = find_root(path, worktree, server.root_markers)
        .ok_or_else(|| format!("could not find project root for {}", path.display()))?;
    let key = format!("{}::{}", server.id, root.display());

    {
        let mut inner = manager.inner.lock().await;
        if inner.broken_servers.contains(&key) {
            return Err(format!("LSP server {} is broken, skipping", server.id));
        }
        if !inner.clients.contains_key(&key) {
            match spawn_client(&server, &root).await {
                Ok(client) => {
                    inner.clients.insert(key.clone(), client);
                }
                Err(e) => {
                    inner.broken_servers.insert(key);
                    return Err(e);
                }
            }
        }
    }

    let inner = manager.inner.lock().await;
    let client = inner
        .clients
        .get(&key)
        .ok_or_else(|| "client disappeared".to_string())?;
    let (stdin, pending, seq) = clone_client_request_refs(client);
    Ok((stdin, pending, seq, client.opened.clone()))
}

pub(super) async fn hover(
    manager: &LspManager,
    worktree: &Path,
    path: &Path,
    line: u32,
    character: u32,
) -> Result<String, String> {
    let (stdin, pending, seq, opened) = get_request_refs(manager, worktree, path).await?;
    let uri = ensure_did_open(&stdin, path, &opened).await?;

    let result = send_request(
        &stdin,
        &pending,
        &seq,
        "textDocument/hover",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        }),
        REQUEST_TIMEOUT,
    )
    .await?;

    Ok(format_hover_result(&result))
}

pub(super) async fn go_to_definition(
    manager: &LspManager,
    worktree: &Path,
    path: &Path,
    line: u32,
    character: u32,
) -> Result<String, String> {
    let (stdin, pending, seq, opened) = get_request_refs(manager, worktree, path).await?;
    let uri = ensure_did_open(&stdin, path, &opened).await?;

    let result = send_request(
        &stdin,
        &pending,
        &seq,
        "textDocument/definition",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        }),
        REQUEST_TIMEOUT,
    )
    .await?;

    Ok(format_definition_result(&result))
}

pub(super) async fn find_references(
    manager: &LspManager,
    worktree: &Path,
    path: &Path,
    line: u32,
    character: u32,
) -> Result<String, String> {
    let (stdin, pending, seq, opened) = get_request_refs(manager, worktree, path).await?;
    let uri = ensure_did_open(&stdin, path, &opened).await?;

    let result = send_request(
        &stdin,
        &pending,
        &seq,
        "textDocument/references",
        json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "context": { "includeDeclaration": true },
        }),
        REQUEST_TIMEOUT,
    )
    .await?;

    Ok(format_references_result(&result))
}

pub(super) async fn document_symbols(
    manager: &LspManager,
    worktree: &Path,
    path: &Path,
    query: SymbolQuery,
) -> Result<String, String> {
    let symbols = fetch_document_symbols(manager, worktree, path).await?;
    if symbols.is_empty() {
        return Ok("No symbols found in this document.".to_string());
    }

    Ok(format_document_symbols(&symbols, &query))
}

pub(super) async fn hover_symbol(
    manager: &LspManager,
    worktree: &Path,
    path: &Path,
    symbol_query: &str,
) -> Result<String, String> {
    let (line, character) =
        resolve_symbol_to_position(manager, worktree, path, symbol_query).await?;
    hover(manager, worktree, path, line, character).await
}

pub(super) async fn go_to_definition_symbol(
    manager: &LspManager,
    worktree: &Path,
    path: &Path,
    symbol_query: &str,
) -> Result<String, String> {
    let (line, character) =
        resolve_symbol_to_position(manager, worktree, path, symbol_query).await?;
    go_to_definition(manager, worktree, path, line, character).await
}

pub(super) async fn find_references_symbol(
    manager: &LspManager,
    worktree: &Path,
    path: &Path,
    symbol_query: &str,
) -> Result<String, String> {
    let (line, character) =
        resolve_symbol_to_position(manager, worktree, path, symbol_query).await?;
    find_references(manager, worktree, path, line, character).await
}

fn format_hover_result(result: &serde_json::Value) -> String {
    if result.is_null() {
        return "No hover information available at this position.".to_string();
    }

    let contents = result.get("contents").unwrap_or(result);
    if let Some(s) = contents.as_str() {
        s.to_string()
    } else if let Some(obj) = contents.as_object() {
        obj.get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    } else if let Some(arr) = contents.as_array() {
        arr.iter()
            .filter_map(|item| {
                if let Some(s) = item.as_str() {
                    Some(s.to_string())
                } else {
                    item.get("value").and_then(|v| v.as_str()).map(String::from)
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    } else {
        format!("{contents}")
    }
}

fn format_definition_result(result: &serde_json::Value) -> String {
    if result.is_null() {
        return "No definition found at this position.".to_string();
    }

    let locations = if result.is_array() {
        result.as_array().cloned().unwrap_or_default()
    } else {
        vec![result.clone()]
    };

    if locations.is_empty() {
        return "No definition found at this position.".to_string();
    }

    locations
        .iter()
        .map(format_location)
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_references_result(result: &serde_json::Value) -> String {
    if result.is_null() {
        return "No references found at this position.".to_string();
    }

    let locations = result.as_array().cloned().unwrap_or_default();
    if locations.is_empty() {
        return "No references found at this position.".to_string();
    }

    let mut formatted: Vec<String> = locations.iter().map(format_location).collect();
    let total = formatted.len();
    formatted.truncate(50);
    let mut out = formatted.join("\n");
    if total > 50 {
        out.push_str(&format!("\n… and {} more references", total - 50));
    }
    out
}

fn format_location(loc: &serde_json::Value) -> String {
    let uri = loc
        .get("uri")
        .or_else(|| loc.get("targetUri"))
        .and_then(|u| u.as_str())
        .unwrap_or("?");
    let range = loc.get("range").or_else(|| loc.get("targetSelectionRange"));
    let (line, character) = match range {
        Some(r) => {
            let start = r.get("start").unwrap_or(r);
            let l = start.get("line").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
            let c = start.get("character").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
            (l, c)
        }
        None => (1, 1),
    };
    let file = uri.strip_prefix("file://").unwrap_or(uri);
    format!("{file}:{line}:{character}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        format_definition_result, format_hover_result, format_location, format_references_result,
    };

    #[test]
    fn hover_formats_markup_content() {
        let value = json!({ "contents": { "kind": "markdown", "value": "```rs\nfn demo()\n```" } });
        assert_eq!(format_hover_result(&value), "```rs\nfn demo()\n```");
    }

    #[test]
    fn hover_formats_marked_string_arrays() {
        let value = json!({
            "contents": [
                "plain text",
                { "language": "rust", "value": "fn demo()" }
            ]
        });
        assert_eq!(format_hover_result(&value), "plain text\n\nfn demo()");
    }

    #[test]
    fn definition_formats_single_and_plural_locations() {
        let single = json!({
            "uri": "file:///tmp/demo.rs",
            "range": { "start": { "line": 2, "character": 3 } }
        });
        let plural = json!([
            single.clone(),
            {
                "targetUri": "file:///tmp/lib.rs",
                "targetSelectionRange": { "start": { "line": 0, "character": 0 } }
            }
        ]);

        assert_eq!(format_definition_result(&single), "/tmp/demo.rs:3:4");
        assert_eq!(
            format_definition_result(&plural),
            "/tmp/demo.rs:3:4\n/tmp/lib.rs:1:1"
        );
    }

    #[test]
    fn references_truncate_after_fifty_entries() {
        let refs = json!(
            (0..55)
                .map(|i| json!({
                    "uri": format!("file:///tmp/ref-{i}.rs"),
                    "range": { "start": { "line": i, "character": 0 } }
                }))
                .collect::<Vec<_>>()
        );

        let output = format_references_result(&refs);
        assert!(output.contains("/tmp/ref-0.rs:1:1"));
        assert!(output.contains("/tmp/ref-49.rs:50:1"));
        assert!(!output.contains("/tmp/ref-54.rs:55:1"));
        assert!(output.ends_with("… and 5 more references"));
    }

    #[test]
    fn location_formats_target_uri_fallback() {
        let loc = json!({
            "targetUri": "file:///tmp/target.rs",
            "targetSelectionRange": { "start": { "line": 9, "character": 1 } }
        });
        assert_eq!(format_location(&loc), "/tmp/target.rs:10:2");
    }
}
