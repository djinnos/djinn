use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, sleep};

use crate::client::{LspClient, clone_client_refs, kill_client, spawn_client, write_lsp_message};
use crate::diagnostics::{Diagnostic, clear_uri, collect_for_worktree};
use crate::requests;
use crate::server_config::{language_id_for_path, server_for_path};
use crate::symbols::SymbolQuery;
use crate::workspace::find_root;

#[derive(Debug, Clone)]
pub struct LspWarning {
    /// e.g. "rust-analyzer", "typescript-language-server"
    pub server: String,
    /// Human-readable install instructions.
    pub message: String,
}

#[derive(Clone)]
pub struct LspManager {
    pub(crate) inner: Arc<Mutex<LspInner>>,
}

pub(crate) struct LspInner {
    pub(crate) clients: HashMap<String, LspClient>,
    pub(crate) broken_servers: HashSet<String>,
    /// Warnings for missing LSP servers, surfaced to the user via board_health.
    warnings: Vec<LspWarning>,
}

impl Default for LspManager {
    fn default() -> Self {
        Self::new()
    }
}

impl LspManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(LspInner {
                clients: HashMap::new(),
                broken_servers: HashSet::new(),
                warnings: Vec::new(),
            })),
        }
    }

    pub async fn shutdown_all(&self) {
        let mut inner = self.inner.lock().await;
        for (_, client) in inner.clients.drain() {
            kill_client(client);
        }
    }

    /// Kill all LSP clients whose project root is under `worktree`.
    /// Must be called before removing a worktree directory.
    /// Returns the number of clients killed.
    pub async fn shutdown_for_worktree(&self, worktree: &Path) -> usize {
        let worktree_str = worktree.to_string_lossy();
        let mut inner = self.inner.lock().await;
        let keys: Vec<String> = inner
            .clients
            .keys()
            .filter(|key| {
                key.split_once("::")
                    .map(|(_, root)| root.starts_with(worktree_str.as_ref()))
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        let mut killed = 0;
        for key in keys {
            if let Some(client) = inner.clients.remove(&key) {
                tracing::info!(key = %key, pid = client.pid, "lsp: killing client for worktree teardown");
                kill_client(client);
                killed += 1;
            }
        }
        killed
    }

    #[allow(clippy::disallowed_methods)] // scoped: direct wall-clock read; migration tracked by lint-ratchet task 70y0 (Clock abstraction already lands in 8bcj/m5g4)
    pub async fn touch_file(&self, worktree: &Path, path: &Path, wait_for_diagnostics: bool) {
        let Some(server) = server_for_path(path) else {
            tracing::debug!(path = %path.display(), "lsp: no server configured for file extension");
            return;
        };

        let Some(root) = find_root(path, worktree, server.root_markers, server.root_strategy)
        else {
            tracing::warn!(path = %path.display(), server = server.id, "lsp: could not find project root");
            return;
        };

        let key = format!("{}::{}", server.id, root.display());

        {
            let mut inner = self.inner.lock().await;
            if inner.broken_servers.contains(&key) {
                tracing::debug!(key = %key, "lsp: skipping broken server");
                return;
            }
            if !inner.clients.contains_key(&key) {
                tracing::info!(server = server.id, root = %root.display(), "lsp: spawning new LSP client");
                match spawn_client(&server, &root).await {
                    Ok(client) => {
                        tracing::info!(server = server.id, "lsp: client spawned successfully");
                        inner.clients.insert(key.clone(), client);
                    }
                    Err(e) => {
                        tracing::error!(server = server.id, error = %e, "lsp: failed to spawn client");
                        if !inner.warnings.iter().any(|w| w.server == server.id) {
                            inner.warnings.push(LspWarning {
                                server: server.id.to_string(),
                                message: e.clone(),
                            });
                        }
                        inner.broken_servers.insert(key);
                        return;
                    }
                }
            }
        }

        let client = {
            let inner = self.inner.lock().await;
            inner.clients.get(&key).map(clone_client_refs)
        };
        let Some((stdin, diagnostics, opened, ready)) = client else {
            return;
        };

        let uri = format!("file://{}", path.display());
        let text = match tokio::fs::read_to_string(path).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "lsp: could not read file for touch");
                return;
            }
        };

        let mut opened_guard = opened.lock().await;
        let prev_version = opened_guard.get(&uri).copied();

        if let Some(version) = prev_version {
            let next = version + 1;
            opened_guard.insert(uri.clone(), next);
            drop(opened_guard);

            tracing::info!(uri = %uri, version = next, "lsp: sending didChange");

            clear_uri(&diagnostics, &uri).await;

            let watched = json!({
                "jsonrpc": "2.0",
                "method": "workspace/didChangeWatchedFiles",
                "params": {
                    "changes": [{ "uri": uri, "type": 2 }]
                }
            });
            let _ = write_lsp_message(&stdin, &watched.to_string()).await;

            let change = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": next },
                    "contentChanges": [{ "text": text }],
                }
            });
            let _ = write_lsp_message(&stdin, &change.to_string()).await;
        } else {
            let lang = language_id_for_path(path).unwrap_or("plaintext");
            opened_guard.insert(uri.clone(), 0);
            drop(opened_guard);

            tracing::info!(uri = %uri, lang = lang, "lsp: sending didOpen (first touch)");

            clear_uri(&diagnostics, &uri).await;

            let watched = json!({
                "jsonrpc": "2.0",
                "method": "workspace/didChangeWatchedFiles",
                "params": {
                    "changes": [{ "uri": uri, "type": 1 }]
                }
            });
            let _ = write_lsp_message(&stdin, &watched.to_string()).await;

            let open = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri,
                        "languageId": lang,
                        "version": 0,
                        "text": text,
                    }
                }
            });
            let _ = write_lsp_message(&stdin, &open.to_string()).await;
        }

        if wait_for_diagnostics {
            // While the server is still indexing it cannot produce
            // diagnostics for this file — waiting just stalls every
            // write/edit/patch by the full timeout for nothing.
            if !ready.load(Ordering::SeqCst) {
                tracing::info!(uri = %uri, "lsp: server still indexing, skipping diagnostic wait");
                return;
            }
            let start = Instant::now();
            let deadline = start + Duration::from_secs(3);
            let debounce = Duration::from_millis(150);
            let mut last_change = Instant::now();
            let mut prev_snapshot: Option<usize> = None;

            loop {
                let now = Instant::now();
                if now >= deadline {
                    tracing::info!(
                        uri = %uri,
                        elapsed_ms = start.elapsed().as_millis() as u64,
                        final_count = prev_snapshot.unwrap_or(0),
                        "lsp: diagnostic wait timed out (3s)"
                    );
                    break;
                }

                let current_len = {
                    let map = diagnostics.lock().await;
                    map.get(&uri).map(Vec::len)
                };

                match (prev_snapshot, current_len) {
                    (None, None) => {}
                    (None, Some(len)) => {
                        tracing::info!(
                            uri = %uri,
                            count = len,
                            elapsed_ms = start.elapsed().as_millis() as u64,
                            "lsp: first diagnostics arrived"
                        );
                        prev_snapshot = Some(len);
                        last_change = Instant::now();
                    }
                    (Some(prev), Some(len)) if prev != len => {
                        tracing::debug!(uri = %uri, prev = prev, now = len, "lsp: diagnostic count changed");
                        prev_snapshot = Some(len);
                        last_change = Instant::now();
                    }
                    (Some(_), Some(_)) => {
                        if now.duration_since(last_change) >= debounce {
                            tracing::info!(
                                uri = %uri,
                                count = prev_snapshot.unwrap_or(0),
                                elapsed_ms = start.elapsed().as_millis() as u64,
                                "lsp: diagnostics settled after debounce"
                            );
                            break;
                        }
                    }
                    (Some(_), None) => {
                        tracing::debug!(uri = %uri, "lsp: diagnostics were cleared unexpectedly");
                        prev_snapshot = None;
                        last_change = Instant::now();
                    }
                }

                sleep(Duration::from_millis(25)).await;
            }
        }
    }

    /// Return diagnostics scoped to a specific worktree path.
    /// Only returns diagnostics whose file URI starts with the worktree prefix,
    /// preventing cross-project leakage since LspManager is a singleton.
    pub async fn diagnostics(&self, worktree: &Path) -> Vec<Diagnostic> {
        let (client_count, clients) = {
            let inner = self.inner.lock().await;
            let count = inner.clients.len();
            let maps = inner
                .clients
                .values()
                .map(|c| c.diagnostics.clone())
                .collect::<Vec<_>>();
            (count, maps)
        };

        let out = collect_for_worktree(&clients, worktree).await;
        let errors = out.iter().filter(|d| d.severity == 1).count();
        tracing::info!(
            clients = client_count,
            worktree = %worktree.display(),
            total = out.len(),
            errors = errors,
            "lsp: diagnostics() called"
        );
        out
    }

    /// True if any LSP client rooted under `worktree` is still indexing
    /// (its diagnostics are not yet trustworthy).
    pub async fn indexing(&self, worktree: &Path) -> bool {
        let worktree_str = worktree.to_string_lossy();
        let inner = self.inner.lock().await;
        inner.clients.iter().any(|(key, client)| {
            key.split_once("::")
                .map(|(_, root)| root.starts_with(worktree_str.as_ref()))
                .unwrap_or(false)
                && !client.ready.load(Ordering::SeqCst)
        })
    }

    /// Diagnostics for `worktree` formatted for tool responses. While a
    /// server is still indexing, an explicit status note replaces the
    /// silently-empty (and misleading) result.
    pub async fn diagnostics_xml(&self, worktree: &Path) -> String {
        let mut xml = crate::diagnostics::format_diagnostics_xml(self.diagnostics(worktree).await);
        if self.indexing(worktree).await {
            xml.push_str(
                "<diagnostics-status>language server is still indexing; \
                 diagnostics may be missing or incomplete</diagnostics-status>\n",
            );
        }
        xml
    }

    /// Return any warnings about missing/broken LSP servers.
    /// These are surfaced to the user via board_health so they can install them.
    pub async fn warnings(&self) -> Vec<LspWarning> {
        self.inner.lock().await.warnings.clone()
    }

    pub async fn hover(
        &self,
        worktree: &Path,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<String, String> {
        requests::hover(self, worktree, path, line, character).await
    }

    pub async fn go_to_definition(
        &self,
        worktree: &Path,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<String, String> {
        requests::go_to_definition(self, worktree, path, line, character).await
    }

    pub async fn find_references(
        &self,
        worktree: &Path,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<String, String> {
        requests::find_references(self, worktree, path, line, character).await
    }

    pub async fn document_symbols(
        &self,
        worktree: &Path,
        path: &Path,
        query: SymbolQuery,
    ) -> Result<String, String> {
        requests::document_symbols(self, worktree, path, query).await
    }

    pub async fn hover_symbol(
        &self,
        worktree: &Path,
        path: &Path,
        symbol_query: &str,
    ) -> Result<String, String> {
        requests::hover_symbol(self, worktree, path, symbol_query).await
    }

    pub async fn go_to_definition_symbol(
        &self,
        worktree: &Path,
        path: &Path,
        symbol_query: &str,
    ) -> Result<String, String> {
        requests::go_to_definition_symbol(self, worktree, path, symbol_query).await
    }

    pub async fn find_references_symbol(
        &self,
        worktree: &Path,
        path: &Path,
        symbol_query: &str,
    ) -> Result<String, String> {
        requests::find_references_symbol(self, worktree, path, symbol_query).await
    }
}
