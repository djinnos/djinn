//! Code intelligence handlers: `lsp`.
//!
//! `code_graph` requires complex bridge operations and remains as
//! `Unhandled` in the extension dispatch for the djinn-agent façade.
//! `github_search` and `github_fetch_file` are routed through
//! `SupervisorServices`.

use crate::context::ExtensionContext;
use crate::helpers::*;
use crate::types::*;

/// Dispatch an LSP operation (hover, definition, references, symbols).
pub(crate) async fn call_lsp(
    ctx: &dyn ExtensionContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &std::path::Path,
) -> Result<serde_json::Value, String> {
    let p: LspParams = parse_args(arguments)?;
    validate_symbol_only_params(&p.operation, &p)?;
    let path = resolve_path(&p.file_path, worktree_path);
    let lsp = ctx.lsp();

    match p.operation.as_str() {
        "hover" => {
            let result = match (&p.symbol, p.line, p.character) {
                (Some(symbol), None, None) => {
                    lsp.hover_symbol(worktree_path, &path, symbol).await?
                }
                (None, Some(line), Some(character)) => {
                    lsp.hover(
                        worktree_path,
                        &path,
                        line.saturating_sub(1),
                        character.saturating_sub(1),
                    )
                    .await?
                }
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(
                        "hover accepts either symbol or line+character, but not both".to_string(),
                    );
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(
                        "hover requires both line and character when symbol is omitted".to_string(),
                    );
                }
                (None, None, None) => {
                    return Err("hover requires either symbol or line+character".to_string());
                }
            };
            Ok(serde_json::json!({ "operation": "hover", "result": result }))
        }
        "definition" => {
            let result = match (&p.symbol, p.line, p.character) {
                (Some(symbol), None, None) => {
                    lsp.go_to_definition_symbol(worktree_path, &path, symbol)
                        .await?
                }
                (None, Some(line), Some(character)) => {
                    lsp.go_to_definition(
                        worktree_path,
                        &path,
                        line.saturating_sub(1),
                        character.saturating_sub(1),
                    )
                    .await?
                }
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(
                        "definition accepts either symbol or line+character, but not both"
                            .to_string(),
                    );
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(
                        "definition requires both line and character when symbol is omitted"
                            .to_string(),
                    );
                }
                (None, None, None) => {
                    return Err("definition requires either symbol or line+character".to_string());
                }
            };
            Ok(serde_json::json!({ "operation": "definition", "result": result }))
        }
        "references" => {
            let result = match (&p.symbol, p.line, p.character) {
                (Some(symbol), None, None) => {
                    lsp.find_references_symbol(worktree_path, &path, symbol)
                        .await?
                }
                (None, Some(line), Some(character)) => {
                    lsp.find_references(
                        worktree_path,
                        &path,
                        line.saturating_sub(1),
                        character.saturating_sub(1),
                    )
                    .await?
                }
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(
                        "references accepts either symbol or line+character, but not both"
                            .to_string(),
                    );
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(
                        "references requires both line and character when symbol is omitted"
                            .to_string(),
                    );
                }
                (None, None, None) => {
                    return Err("references requires either symbol or line+character".to_string());
                }
            };
            Ok(serde_json::json!({ "operation": "references", "result": result }))
        }
        "symbols" => {
            let query = djinn_lsp::SymbolQuery {
                depth: p.depth,
                kinds: p
                    .kind
                    .as_deref()
                    .map(djinn_lsp::parse_symbol_kind_filter)
                    .transpose()?,
                name_filter: p.name_filter,
            };
            let result = lsp.document_symbols(worktree_path, &path, query).await?;
            Ok(serde_json::json!({ "operation": "symbols", "result": result }))
        }
        other => Err(format!(
            "unknown LSP operation: {other}. Use: hover, definition, references, or symbols"
        )),
    }
}
