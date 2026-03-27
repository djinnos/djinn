pub use diagnostics::{Diagnostic, format_diagnostics_xml};

mod client;
mod diagnostics;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use client::{
    ClientStdin, LspClient, OpenedFiles, PendingResponses, clone_client_refs,
    clone_client_request_refs, ensure_did_open, kill_client, send_request, spawn_client,
    write_lsp_message,
};
use diagnostics::{clear_uri, collect_for_worktree};
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, sleep};

/// Timeout for LSP `initialize` — rust-analyzer can take 30-45s on first run
/// while it builds its index.  Matches opencode's 45s timeout.
const INIT_TIMEOUT: Duration = Duration::from_secs(45);

/// Timeout for regular LSP requests (hover, definition, references, symbols).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct LspWarning {
    /// e.g. "rust-analyzer", "typescript-language-server"
    pub server: String,
    /// Human-readable install instructions.
    pub message: String,
}

#[derive(Clone)]
pub struct LspManager {
    inner: Arc<Mutex<LspInner>>,
}

struct LspInner {
    clients: HashMap<String, LspClient>,
    broken_servers: std::collections::HashSet<String>,
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
                broken_servers: std::collections::HashSet::new(),
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
    pub async fn shutdown_for_worktree(&self, worktree: &Path) {
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
        for key in keys {
            if let Some(client) = inner.clients.remove(&key) {
                tracing::info!(key = %key, pid = client.pid, "lsp: killing client for worktree teardown");
                kill_client(client);
            }
        }
    }

    pub async fn touch_file(&self, worktree: &Path, path: &Path, wait_for_diagnostics: bool) {
        let Some(server) = server_for_path(path) else {
            tracing::debug!(path = %path.display(), "lsp: no server configured for file extension");
            return;
        };

        let Some(root) = find_root(path, worktree, server.root_markers) else {
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
                        // Add a user-facing warning if not already present.
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
        let Some((stdin, diagnostics, opened)) = client else {
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
            // File already open — notify LSP that the file changed on disk,
            // then send didChange with incremented version.
            let next = version + 1;
            opened_guard.insert(uri.clone(), next);
            drop(opened_guard);

            tracing::info!(uri = %uri, version = next, "lsp: sending didChange");

            // Clear stale diagnostics so the debounce loop waits for fresh ones.
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
            // First time opening this file.
            let lang = language_id_for_path(path).unwrap_or("plaintext");
            opened_guard.insert(uri.clone(), 0);
            drop(opened_guard);

            tracing::info!(uri = %uri, lang = lang, "lsp: sending didOpen (first touch)");

            // Clear any stale diagnostics from a previous session.
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
                    // No diagnostics yet — keep waiting for initial arrival
                    (None, None) => {}
                    // First diagnostics arrived — reset debounce
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
                    // Count changed — reset debounce
                    (Some(prev), Some(len)) if prev != len => {
                        tracing::debug!(uri = %uri, prev = prev, now = len, "lsp: diagnostic count changed");
                        prev_snapshot = Some(len);
                        last_change = Instant::now();
                    }
                    // Diagnostics present but unchanged — check debounce expiry
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
                    // Diagnostics cleared (shouldn't happen normally)
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

    /// Return any warnings about missing/broken LSP servers.
    /// These are surfaced to the user via board_health so they can install them.
    pub async fn warnings(&self) -> Vec<LspWarning> {
        self.inner.lock().await.warnings.clone()
    }
}

struct ServerDef {
    id: &'static str,
    /// The binary name (first element) and fixed args.
    cmd: &'static [&'static str],
    root_markers: &'static [&'static str],
    /// How to install this server if it's not found on PATH.
    install: InstallMethod,
}

#[derive(Clone, Copy)]
enum InstallMethod {
    /// Install via `npm install -g <packages..>` into ~/.djinn/bin
    Npm(&'static [&'static str]),
    /// Install via `rustup component add`, or download from GitHub releases.
    /// Fields: (component_name, github_repo, asset_pattern_fragment)
    /// asset_pattern_fragment is matched against release asset names (e.g. "x86_64-unknown-linux-gnu").
    RustAnalyzer,
    /// Install via `go install <pkg>@latest` with GOBIN=~/.djinn/bin
    GoInstall(&'static str),
}

fn server_for_path(path: &Path) -> Option<ServerDef> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some(ServerDef {
            id: "rust-analyzer",
            cmd: &["rust-analyzer"],
            root_markers: &["Cargo.toml"],
            install: InstallMethod::RustAnalyzer,
        }),
        Some("go") => Some(ServerDef {
            id: "gopls",
            cmd: &["gopls"],
            root_markers: &["go.mod"],
            install: InstallMethod::GoInstall("golang.org/x/tools/gopls"),
        }),
        Some("ts") | Some("tsx") | Some("js") | Some("jsx") => Some(ServerDef {
            id: "typescript-language-server",
            cmd: &["typescript-language-server", "--stdio"],
            root_markers: &["package.json", "tsconfig.json"],
            install: InstallMethod::Npm(&["typescript-language-server", "typescript"]),
        }),
        Some("py") => Some(ServerDef {
            id: "pyright",
            cmd: &["pyright-langserver", "--stdio"],
            root_markers: &["pyproject.toml", "setup.py"],
            install: InstallMethod::Npm(&["pyright"]),
        }),
        _ => None,
    }
}

/// Djinn-managed binary directory for auto-installed LSP servers.
fn djinn_bin_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share")
        })
        .join("djinn")
        .join("bin")
}

/// Resolve the binary for an LSP server: check PATH (augmented with
/// ~/.djinn/bin), and auto-install if missing.
async fn resolve_binary(server: &ServerDef) -> Result<PathBuf, String> {
    let bin_dir = djinn_bin_dir();
    let system_path = std::env::var("PATH").unwrap_or_default();
    resolve_binary_inner(server, &bin_dir, &system_path)
}

/// Core resolution logic, factored out for testing.
fn resolve_binary_inner(
    server: &ServerDef,
    bin_dir: &Path,
    system_path: &str,
) -> Result<PathBuf, String> {
    let binary_name = server.cmd[0];

    // Build an augmented PATH that includes our managed bin dir.
    let augmented_path = format!("{}:{}", bin_dir.display(), system_path);

    // Check if the binary already exists (on PATH or in our bin dir).
    if let Some(found) = which_in_path(binary_name, &augmented_path) {
        tracing::debug!(binary = binary_name, path = %found.display(), "lsp: binary found");
        return Ok(found);
    }

    // For rustup-managed binaries, system-packaged rustup (e.g. Arch) doesn't
    // create proxy shims on PATH.  Ask `rustup which` before attempting install.
    if matches!(server.install, InstallMethod::RustAnalyzer)
        && let Some(rustup) = which_in_path("rustup", system_path)
        && let Some(o) = std::process::Command::new(rustup)
            .args(["which", binary_name])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .ok()
        && o.status.success()
    {
        let p = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string());
        if p.is_file() {
            tracing::debug!(binary = binary_name, path = %p.display(), "lsp: found via rustup which");
            return Ok(p);
        }
    }

    // Not found — attempt installation.
    tracing::info!(
        server = server.id,
        binary = binary_name,
        "lsp: binary not found, attempting auto-install"
    );

    std::fs::create_dir_all(bin_dir)
        .map_err(|e| format!("failed to create {}: {e}", bin_dir.display()))?;

    match server.install {
        InstallMethod::Npm(packages) => {
            // Find npm
            let npm = which_in_path("npm", system_path)
                .ok_or_else(|| "npm not found — cannot auto-install LSP server".to_string())?;

            let mut cmd = std::process::Command::new(npm);
            cmd.arg("install")
                .arg("-g")
                .arg(format!("--prefix={}", bin_dir.display()));
            for pkg in packages {
                cmd.arg(*pkg);
            }
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

            tracing::info!(packages = ?packages, prefix = %bin_dir.display(), "lsp: running npm install");
            let output = cmd
                .output()
                .map_err(|e| format!("npm install failed: {e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("npm install failed: {stderr}"));
            }
        }
        InstallMethod::RustAnalyzer => {
            if let Some(rustup) = which_in_path("rustup", system_path) {
                tracing::info!("lsp: running rustup component add rust-analyzer");
                let output = std::process::Command::new(&rustup)
                    .args(["component", "add", "rust-analyzer"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .map_err(|e| format!("rustup failed: {e}"))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!("rustup component add failed: {stderr}"));
                }

                // System-packaged rustup (e.g. Arch `pacman -S rustup`) doesn't
                // create proxy shims in ~/.cargo/bin, so the binary won't appear
                // on PATH even after a successful component add.  Ask rustup
                // directly where the binary lives and return early if found.
                if which_in_path(binary_name, &augmented_path).is_none()
                    && let Some(o) = std::process::Command::new(&rustup)
                        .args(["which", binary_name])
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .output()
                        .ok()
                    && o.status.success()
                {
                    let p = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string());
                    if p.is_file() {
                        tracing::info!(path = %p.display(), "lsp: resolved via rustup which");
                        return Ok(p);
                    }
                }
            } else {
                // rust-analyzer isn't a standalone crate — can't cargo install it.
                // Must be installed via rustup or system package manager.
                return Err(
                    "rust-analyzer not found — install via `rustup component add rust-analyzer` \
                     or your system package manager (e.g. `pacman -S rust-analyzer`)"
                        .to_string(),
                );
            }
        }
        InstallMethod::GoInstall(pkg) => {
            let go = which_in_path("go", system_path)
                .ok_or_else(|| "go not found — cannot auto-install gopls".to_string())?;

            tracing::info!(pkg = pkg, gobin = %bin_dir.display(), "lsp: running go install");
            let output = std::process::Command::new(go)
                .args(["install", &format!("{pkg}@latest")])
                .env("GOBIN", bin_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .map_err(|e| format!("go install failed: {e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("go install failed: {stderr}"));
            }
        }
    }

    // Verify the binary is now available.
    which_in_path(binary_name, &augmented_path).ok_or_else(|| {
        format!(
            "installed {} but binary '{}' still not found in PATH or {}",
            server.id,
            binary_name,
            bin_dir.display()
        )
    })
}

/// Simple which(1) — scan colon-delimited PATH for an executable.
fn which_in_path(binary: &str, path_var: &str) -> Option<PathBuf> {
    for dir in path_var.split(':') {
        let candidate = Path::new(dir).join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
        // npm --prefix puts binaries under bin/
        let npm_candidate = Path::new(dir).join("bin").join(binary);
        if npm_candidate.is_file() {
            return Some(npm_candidate);
        }
    }
    None
}

fn language_id_for_path(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some("rust"),
        Some("go") => Some("go"),
        Some("py") => Some("python"),
        Some("ts") => Some("typescript"),
        Some("tsx") => Some("typescriptreact"),
        Some("js") => Some("javascript"),
        Some("jsx") => Some("javascriptreact"),
        Some("json") => Some("json"),
        Some("toml") => Some("toml"),
        Some("yaml") | Some("yml") => Some("yaml"),
        Some("md") => Some("markdown"),
        _ => None,
    }
}

fn find_root(path: &Path, worktree: &Path, sentinels: &[&str]) -> Option<PathBuf> {
    let mut cur = path.parent()?.to_path_buf();
    loop {
        for s in sentinels {
            if cur.join(s).exists() {
                return Some(cur.clone());
            }
        }
        if cur == worktree {
            return Some(worktree.to_path_buf());
        }
        if !cur.pop() {
            return Some(worktree.to_path_buf());
        }
    }
}

/// Format an LSP Location or LocationLink into a human-readable string.
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

#[derive(Debug, Clone, Default)]
pub struct SymbolQuery {
    pub depth: Option<usize>,
    pub kinds: Option<HashSet<u64>>,
    pub name_filter: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SymbolEntry {
    kind_name: &'static str,
    name_path: String,
    line: Option<u64>,
    location: Option<String>,
    child_count: usize,
}

fn parse_symbol_tree(symbols: &[serde_json::Value], query: &SymbolQuery) -> Vec<SymbolEntry> {
    let normalized_name_filter = query.name_filter.as_ref().map(|value| value.to_lowercase());
    let mut entries = Vec::new();
    for symbol in symbols {
        collect_symbol_entries(
            symbol,
            0,
            &mut Vec::new(),
            &normalized_name_filter,
            query,
            &mut entries,
        );
    }
    entries
}

fn collect_symbol_entries(
    symbol: &serde_json::Value,
    depth: usize,
    parent_path: &mut Vec<String>,
    normalized_name_filter: &Option<String>,
    query: &SymbolQuery,
    entries: &mut Vec<SymbolEntry>,
) {
    let name = symbol
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("?")
        .to_string();
    let kind_num = symbol
        .get("kind")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);

    parent_path.push(name.clone());
    let name_path = parent_path.join("/");

    let matches_depth = query.depth.is_none_or(|max_depth| depth <= max_depth);
    let matches_kind = query
        .kinds
        .as_ref()
        .is_none_or(|kinds| kinds.contains(&kind_num));
    let matches_name = normalized_name_filter.as_ref().is_none_or(|needle| {
        let lowered_name = name.to_lowercase();
        let lowered_path = name_path.to_lowercase();
        lowered_name.contains(needle) || lowered_path.contains(needle)
    });

    if matches_depth && matches_kind && matches_name {
        let line = symbol
            .get("range")
            .and_then(|range| range.get("start"))
            .and_then(|start| start.get("line"))
            .and_then(|line| line.as_u64())
            .map(|line| line + 1);
        let location = symbol.get("location").map(format_location);
        let child_count = symbol
            .get("children")
            .and_then(|value| value.as_array())
            .map(Vec::len)
            .unwrap_or(0);
        entries.push(SymbolEntry {
            kind_name: symbol_kind_name(kind_num),
            name_path,
            line,
            location,
            child_count,
        });
    }

    if let Some(children) = symbol.get("children").and_then(|value| value.as_array()) {
        for child in children {
            collect_symbol_entries(
                child,
                depth + 1,
                parent_path,
                normalized_name_filter,
                query,
                entries,
            );
        }
    }

    parent_path.pop();
}

fn format_symbol_entries(entries: &[SymbolEntry]) -> String {
    if entries.is_empty() {
        return "No symbols found in this document.".to_string();
    }

    let full = render_grouped_symbols(entries, SymbolRenderMode::Full);
    if full.len() <= 8_000 {
        return full;
    }

    let compact = render_grouped_symbols(entries, SymbolRenderMode::ChildCounts);
    if compact.len() <= 8_000 {
        return format!("{compact}\n\n(output shortened: children collapsed to counts)");
    }

    format!(
        "{}\n\n(output shortened: showing kind counts only)",
        render_kind_counts(entries)
    )
}

#[derive(Clone, Copy)]
enum SymbolRenderMode {
    Full,
    ChildCounts,
}

fn render_grouped_symbols(entries: &[SymbolEntry], mode: SymbolRenderMode) -> String {
    let mut groups: BTreeMap<&'static str, Vec<&SymbolEntry>> = BTreeMap::new();
    for entry in entries {
        groups.entry(entry.kind_name).or_default().push(entry);
    }

    let mut sections = Vec::new();
    for (kind, mut group_entries) in groups {
        group_entries.sort_by(|a, b| a.name_path.cmp(&b.name_path).then(a.line.cmp(&b.line)));
        let mut lines = vec![format!("{kind} ({})", group_entries.len())];
        for entry in group_entries {
            let location = entry
                .location
                .clone()
                .or_else(|| entry.line.map(|line| format!("line {line}")))
                .unwrap_or_else(|| "line ?".to_string());
            let suffix = match mode {
                SymbolRenderMode::Full => String::new(),
                SymbolRenderMode::ChildCounts if entry.child_count > 0 => {
                    format!(" [children: {}]", entry.child_count)
                }
                SymbolRenderMode::ChildCounts => String::new(),
            };
            lines.push(format!("- {} ({location}){suffix}", entry.name_path));
        }
        sections.push(lines.join("\n"));
    }

    sections.join("\n\n")
}

fn render_kind_counts(entries: &[SymbolEntry]) -> String {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for entry in entries {
        *counts.entry(entry.kind_name).or_default() += 1;
    }

    let mut lines = vec!["Symbol kinds".to_string()];
    for (kind, count) in counts {
        lines.push(format!("- {kind}: {count}"));
    }
    lines.join("\n")
}

pub fn parse_symbol_kind_filter(value: &str) -> Result<HashSet<u64>, String> {
    let mut kinds = HashSet::new();
    for raw_kind in value.split(',') {
        let normalized = raw_kind.trim().to_lowercase();
        if normalized.is_empty() {
            continue;
        }
        let kind_num = match normalized.as_str() {
            "file" => 1,
            "module" => 2,
            "namespace" => 3,
            "package" => 4,
            "class" => 5,
            "method" => 6,
            "property" => 7,
            "field" => 8,
            "constructor" => 9,
            "enum" => 10,
            "interface" => 11,
            "function" | "fn" => 12,
            "variable" | "var" => 13,
            "constant" | "const" => 14,
            "string" => 15,
            "number" => 16,
            "boolean" | "bool" => 17,
            "array" => 18,
            "object" => 19,
            "key" => 20,
            "null" => 21,
            "enummember" | "enum_member" | "enum-member" => 22,
            "struct" => 23,
            "event" => 24,
            "operator" => 25,
            "typeparameter" | "type_parameter" | "type-parameter" => 26,
            other => return Err(format!("unknown symbol kind filter: {other}")),
        };
        kinds.insert(kind_num);
    }

    if kinds.is_empty() {
        return Err("symbol kind filter must not be empty".to_string());
    }

    Ok(kinds)
}

fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "File",
        2 => "Module",
        3 => "Namespace",
        4 => "Package",
        5 => "Class",
        6 => "Method",
        7 => "Property",
        8 => "Field",
        9 => "Constructor",
        10 => "Enum",
        11 => "Interface",
        12 => "Function",
        13 => "Variable",
        14 => "Constant",
        15 => "String",
        16 => "Number",
        17 => "Boolean",
        18 => "Array",
        19 => "Object",
        20 => "Key",
        21 => "Null",
        22 => "EnumMember",
        23 => "Struct",
        24 => "Event",
        25 => "Operator",
        26 => "TypeParameter",
        _ => "Unknown",
    }
}

impl LspManager {
    /// Get or spawn the LSP client for a file and return refs for making requests.
    async fn get_request_refs(
        &self,
        worktree: &Path,
        path: &Path,
    ) -> Result<(ClientStdin, PendingResponses, Arc<AtomicU64>, OpenedFiles), String> {
        let server = server_for_path(path)
            .ok_or_else(|| format!("no LSP server configured for {}", path.display()))?;
        let root = find_root(path, worktree, server.root_markers)
            .ok_or_else(|| format!("could not find project root for {}", path.display()))?;
        let key = format!("{}::{}", server.id, root.display());

        {
            let mut inner = self.inner.lock().await;
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

        let inner = self.inner.lock().await;
        let client = inner
            .clients
            .get(&key)
            .ok_or_else(|| "client disappeared".to_string())?;
        let (stdin, pending, seq) = clone_client_request_refs(client);
        Ok((stdin, pending, seq, client.opened.clone()))
    }

    /// Send textDocument/hover and return the hover contents as text.
    pub async fn hover(
        &self,
        worktree: &Path,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<String, String> {
        let (stdin, pending, seq, opened) = self.get_request_refs(worktree, path).await?;
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

        if result.is_null() {
            return Ok("No hover information available at this position.".to_string());
        }

        let contents = result.get("contents").unwrap_or(&result);
        // contents can be MarkedString, MarkedString[], or MarkupContent
        let text = if let Some(s) = contents.as_str() {
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
        };

        Ok(text)
    }

    /// Send textDocument/definition and return location(s).
    pub async fn go_to_definition(
        &self,
        worktree: &Path,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<String, String> {
        let (stdin, pending, seq, opened) = self.get_request_refs(worktree, path).await?;
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

        if result.is_null() {
            return Ok("No definition found at this position.".to_string());
        }

        let locations = if result.is_array() {
            result.as_array().cloned().unwrap_or_default()
        } else {
            vec![result]
        };

        if locations.is_empty() {
            return Ok("No definition found at this position.".to_string());
        }

        let formatted: Vec<String> = locations.iter().map(format_location).collect();
        Ok(formatted.join("\n"))
    }

    /// Send textDocument/references and return location(s).
    pub async fn find_references(
        &self,
        worktree: &Path,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<String, String> {
        let (stdin, pending, seq, opened) = self.get_request_refs(worktree, path).await?;
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

        if result.is_null() {
            return Ok("No references found at this position.".to_string());
        }

        let locations = result.as_array().cloned().unwrap_or_default();
        if locations.is_empty() {
            return Ok("No references found at this position.".to_string());
        }

        let mut formatted: Vec<String> = locations.iter().map(format_location).collect();
        let total = formatted.len();
        formatted.truncate(50);
        let mut out = formatted.join("\n");
        if total > 50 {
            out.push_str(&format!("\n… and {} more references", total - 50));
        }
        Ok(out)
    }

    /// Send textDocument/documentSymbol and return formatted symbol list.
    pub async fn document_symbols(
        &self,
        worktree: &Path,
        path: &Path,
        query: SymbolQuery,
    ) -> Result<String, String> {
        let (stdin, pending, seq, opened) = self.get_request_refs(worktree, path).await?;
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
            return Ok("No symbols found in this document.".to_string());
        }

        let symbols = result.as_array().cloned().unwrap_or_default();
        if symbols.is_empty() {
            return Ok("No symbols found in this document.".to_string());
        }

        let entries = parse_symbol_tree(&symbols, &query);
        Ok(format_symbol_entries(&entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::diagnostics::DiagnosticsMap;
    use std::path::PathBuf;

    fn make_diag(file: &str, line: u32, character: u32, severity: u32, msg: &str) -> Diagnostic {
        Diagnostic {
            file: file.to_string(),
            line,
            character,
            severity,
            message: msg.to_string(),
        }
    }

    // --- format_diagnostics_xml ---

    #[test]
    fn format_diagnostics_xml_empty() {
        assert_eq!(format_diagnostics_xml(vec![]), "");
    }

    #[test]
    fn format_diagnostics_xml_filters_non_errors() {
        let diags = vec![
            make_diag("file://a.rs", 1, 1, 2, "warning"),
            make_diag("file://a.rs", 2, 1, 3, "info"),
            make_diag("file://a.rs", 3, 1, 4, "hint"),
        ];
        assert_eq!(format_diagnostics_xml(diags), "");
    }

    #[test]
    fn format_diagnostics_xml_includes_errors() {
        let diags = vec![
            make_diag("file://a.rs", 10, 5, 1, "expected semicolon"),
            make_diag("file://a.rs", 20, 1, 2, "unused variable"),
        ];
        let xml = format_diagnostics_xml(diags);
        assert!(xml.contains("ERROR [10:5] expected semicolon"));
        assert!(!xml.contains("unused variable"));
    }

    #[test]
    fn format_diagnostics_xml_groups_by_file() {
        let diags = vec![
            make_diag("file://b.rs", 1, 1, 1, "err b"),
            make_diag("file://a.rs", 1, 1, 1, "err a"),
        ];
        let xml = format_diagnostics_xml(diags);
        // Files sorted alphabetically
        let a_pos = xml.find("file://a.rs").unwrap();
        let b_pos = xml.find("file://b.rs").unwrap();
        assert!(a_pos < b_pos);
    }

    #[test]
    fn format_diagnostics_xml_truncates_files() {
        let diags: Vec<_> = (0..10)
            .map(|i| make_diag(&format!("file://f{i}.rs"), 1, 1, 1, "err"))
            .collect();
        let xml = format_diagnostics_xml(diags);
        let file_count = xml.matches("<diagnostics file=").count();
        assert_eq!(file_count, 5);
    }

    #[test]
    fn format_diagnostics_xml_truncates_per_file() {
        let diags: Vec<_> = (0..30)
            .map(|i| make_diag("file://a.rs", i, 1, 1, &format!("err {i}")))
            .collect();
        let xml = format_diagnostics_xml(diags);
        let error_count = xml.matches("ERROR").count();
        assert_eq!(error_count, 20);
    }

    // --- server_for_path ---

    #[test]
    fn server_for_rust_file() {
        let s = server_for_path(Path::new("/foo/bar.rs")).unwrap();
        assert_eq!(s.id, "rust-analyzer");
    }

    #[test]
    fn server_for_ts_file() {
        let s = server_for_path(Path::new("/foo/bar.ts")).unwrap();
        assert_eq!(s.id, "typescript-language-server");
    }

    #[test]
    fn server_for_tsx_file() {
        let s = server_for_path(Path::new("/foo/bar.tsx")).unwrap();
        assert_eq!(s.id, "typescript-language-server");
    }

    #[test]
    fn server_for_go_file() {
        let s = server_for_path(Path::new("/foo/bar.go")).unwrap();
        assert_eq!(s.id, "gopls");
    }

    #[test]
    fn server_for_python_file() {
        let s = server_for_path(Path::new("/foo/bar.py")).unwrap();
        assert_eq!(s.id, "pyright");
    }

    #[test]
    fn server_for_unknown_extension() {
        assert!(server_for_path(Path::new("/foo/bar.txt")).is_none());
        assert!(server_for_path(Path::new("/foo/bar")).is_none());
    }

    // --- language_id_for_path ---

    #[test]
    fn language_id_mappings() {
        assert_eq!(language_id_for_path(Path::new("a.rs")), Some("rust"));
        assert_eq!(language_id_for_path(Path::new("a.go")), Some("go"));
        assert_eq!(language_id_for_path(Path::new("a.py")), Some("python"));
        assert_eq!(language_id_for_path(Path::new("a.ts")), Some("typescript"));
        assert_eq!(
            language_id_for_path(Path::new("a.tsx")),
            Some("typescriptreact")
        );
        assert_eq!(language_id_for_path(Path::new("a.js")), Some("javascript"));
        assert_eq!(
            language_id_for_path(Path::new("a.jsx")),
            Some("javascriptreact")
        );
        assert_eq!(language_id_for_path(Path::new("a.json")), Some("json"));
        assert_eq!(language_id_for_path(Path::new("a.toml")), Some("toml"));
        assert_eq!(language_id_for_path(Path::new("a.yaml")), Some("yaml"));
        assert_eq!(language_id_for_path(Path::new("a.yml")), Some("yaml"));
        assert_eq!(language_id_for_path(Path::new("a.md")), Some("markdown"));
        assert_eq!(language_id_for_path(Path::new("a.txt")), None);
    }

    // --- find_root ---

    #[test]
    fn find_root_finds_cargo_toml() {
        // Use the actual project directory which has Cargo.toml
        let worktree = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let file = worktree.join("src/agent/lsp.rs");
        let root = find_root(&file, &worktree, &["Cargo.toml"]);
        assert_eq!(root, Some(worktree));
    }

    #[test]
    fn find_root_falls_back_to_worktree() {
        let worktree = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let file = worktree.join("src/agent/lsp.rs");
        let root = find_root(&file, &worktree, &["nonexistent_marker.xyz"]);
        assert_eq!(root, Some(worktree));
    }

    // --- symbol_kind_name ---

    #[test]
    fn symbol_kind_names() {
        assert_eq!(symbol_kind_name(5), "Class");
        assert_eq!(symbol_kind_name(12), "Function");
        assert_eq!(symbol_kind_name(23), "Struct");
        assert_eq!(symbol_kind_name(99), "Unknown");
    }

    // --- format_location ---

    #[test]
    fn format_location_with_uri_and_range() {
        let loc = json!({
            "uri": "file:///foo/bar.rs",
            "range": {
                "start": { "line": 9, "character": 4 },
                "end": { "line": 9, "character": 10 }
            }
        });
        assert_eq!(format_location(&loc), "/foo/bar.rs:10:5");
    }

    #[test]
    fn format_location_with_target_uri() {
        let loc = json!({
            "targetUri": "file:///foo/bar.rs",
            "targetSelectionRange": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 5 }
            }
        });
        assert_eq!(format_location(&loc), "/foo/bar.rs:1:1");
    }

    // --- LspManager unit tests (no real LSP process) ---

    /// Spawn a harmless `sleep 10` process and wrap it as a fake LspClient for
    /// testing shutdown behaviour without a real LSP server.
    async fn spawn_fake_client(root: &str) -> (String, LspClient) {
        use std::process::Stdio;
        let mut child = tokio::process::Command::new("sleep")
            .arg("10")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sleep must be available for tests");
        let pid = child.id().unwrap_or(0);
        let stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
        let _stdout = child.stdout.take().unwrap();
        let reader_handle = tokio::spawn(async move {
            let _ = child.wait().await;
        });
        let key = format!("test-lsp::{root}");
        let client = LspClient {
            stdin,
            pid,
            reader_handle,
            diagnostics: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            seq: Arc::new(AtomicU64::new(2)),
            opened: Arc::new(Mutex::new(HashMap::new())),
        };
        (key, client)
    }

    #[tokio::test]
    async fn shutdown_for_worktree_removes_matching_clients() {
        let mgr = LspManager::new();
        let temp = crate::test_helpers::test_tempdir("djinn-lsp-worktree-");
        let wt1_buf = temp.path().join("worktree1");
        let wt2_buf = temp.path().join("worktree2");
        let wt1 = wt1_buf.to_string_lossy().to_string();
        let wt2 = wt2_buf.to_string_lossy().to_string();

        let (k1, c1) = spawn_fake_client(&format!("{wt1}/proj")).await;
        let (k2, c2) = spawn_fake_client(&format!("{wt1}/proj2")).await;
        let (k3, c3) = spawn_fake_client(&format!("{wt2}/proj")).await;

        {
            let mut inner = mgr.inner.lock().await;
            inner.clients.insert(k1, c1);
            inner.clients.insert(k2, c2);
            inner.clients.insert(k3, c3);
        }

        assert_eq!(mgr.inner.lock().await.clients.len(), 3);

        mgr.shutdown_for_worktree(wt1_buf.as_path()).await;

        let remaining: Vec<String> = mgr.inner.lock().await.clients.keys().cloned().collect();
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].contains(&wt2), "only wt2 client should remain");
    }

    #[tokio::test]
    async fn shutdown_for_worktree_noop_on_no_match() {
        let mgr = LspManager::new();
        let temp = crate::test_helpers::test_tempdir("djinn-lsp-worktree-");
        let other = temp.path().join("other");
        let nonexistent = temp.path().join("nonexistent");
        let (k, c) = spawn_fake_client(&other.join("proj").to_string_lossy()).await;
        mgr.inner.lock().await.clients.insert(k, c);

        mgr.shutdown_for_worktree(nonexistent.as_path()).await;

        assert_eq!(mgr.inner.lock().await.clients.len(), 1);
    }

    #[tokio::test]
    async fn shutdown_all_kills_all_clients() {
        let mgr = LspManager::new();
        let temp = crate::test_helpers::test_tempdir("djinn-lsp-worktree-");
        let wt = temp.path().join("wt");
        let (k1, c1) = spawn_fake_client(&wt.join("proj").to_string_lossy()).await;
        let (k2, c2) = spawn_fake_client(&wt.join("proj2").to_string_lossy()).await;
        {
            let mut inner = mgr.inner.lock().await;
            inner.clients.insert(k1, c1);
            inner.clients.insert(k2, c2);
        }
        assert_eq!(mgr.inner.lock().await.clients.len(), 2);
        mgr.shutdown_all().await;
        assert_eq!(mgr.inner.lock().await.clients.len(), 0);
    }

    #[tokio::test]
    async fn session_end_leaves_no_clients_for_worktree() {
        // Simulates the lifecycle calling shutdown_for_worktree on session end.
        // After the call, the manager must have no clients for that worktree.
        let mgr = LspManager::new();
        let temp = crate::test_helpers::test_tempdir("djinn-lsp-worktree-");
        let worktree = temp.path().join("task-abc").join("worktree");
        let other = temp.path().join("task-xyz").join("worktree");
        let worktree_str = worktree.to_string_lossy().to_string();
        let other_str = other.to_string_lossy().to_string();

        let (k1, c1) = spawn_fake_client(&format!("{worktree_str}/src")).await;
        let (k2, c2) = spawn_fake_client(&format!("{worktree_str}/tests")).await;
        let (k3, c3) = spawn_fake_client(&format!("{other_str}/src")).await;

        {
            let mut inner = mgr.inner.lock().await;
            inner.clients.insert(k1, c1);
            inner.clients.insert(k2, c2);
            inner.clients.insert(k3, c3);
        }

        // Simulate session end for the first task's worktree.
        mgr.shutdown_for_worktree(worktree.as_path()).await;

        let remaining: Vec<String> = mgr.inner.lock().await.clients.keys().cloned().collect();
        assert!(
            remaining.iter().all(|k| !k.contains(&worktree_str)),
            "no clients should remain for the ended session's worktree"
        );
        assert_eq!(
            remaining.len(),
            1,
            "clients for other worktrees must be untouched"
        );
    }

    #[tokio::test]
    async fn lsp_manager_diagnostics_empty_by_default() {
        let mgr = LspManager::new();
        let temp = crate::test_helpers::test_tempdir("djinn-lsp-worktree-");
        assert!(mgr.diagnostics(temp.path()).await.is_empty());
    }

    #[tokio::test]
    async fn lsp_manager_touch_file_no_server_for_txt() {
        let mgr = LspManager::new();
        let tmp = crate::test_helpers::test_tempdir("djinn-lsp-manager-");
        let file = tmp.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();
        // Should return without error even though no server matches
        mgr.touch_file(tmp.path(), &file, false).await;
        assert!(mgr.diagnostics(tmp.path()).await.is_empty());
    }

    // --- Opened files version tracking (unit-level) ---

    #[tokio::test]
    async fn opened_files_tracks_versions() {
        let opened: OpenedFiles = Arc::new(Mutex::new(HashMap::new()));
        let uri = "file:///test.rs".to_string();

        // First time: not present
        assert!(opened.lock().await.get(&uri).is_none());

        // Simulate first open
        opened.lock().await.insert(uri.clone(), 0);
        assert_eq!(*opened.lock().await.get(&uri).unwrap(), 0);

        // Simulate second touch (didChange)
        let version = *opened.lock().await.get(&uri).unwrap();
        opened.lock().await.insert(uri.clone(), version + 1);
        assert_eq!(*opened.lock().await.get(&uri).unwrap(), 1);

        // Third touch
        let version = *opened.lock().await.get(&uri).unwrap();
        opened.lock().await.insert(uri.clone(), version + 1);
        assert_eq!(*opened.lock().await.get(&uri).unwrap(), 2);
    }

    // --- Diagnostics clearing on re-touch ---

    #[tokio::test]
    async fn diagnostics_cleared_before_retouch() {
        let diagnostics: DiagnosticsMap = Arc::new(Mutex::new(HashMap::new()));
        let uri = "file:///test.rs".to_string();

        // Simulate initial diagnostics from first didOpen
        diagnostics
            .lock()
            .await
            .insert(uri.clone(), vec![make_diag(&uri, 1, 1, 1, "old error")]);
        assert_eq!(diagnostics.lock().await.get(&uri).unwrap().len(), 1);

        // Simulate clearing before re-touch (what touch_file now does)
        clear_uri(&diagnostics, &uri).await;
        assert!(diagnostics.lock().await.get(&uri).is_none());
    }

    // --- which_in_path ---

    #[test]
    fn which_in_path_finds_existing_binary() {
        let tmp = crate::test_helpers::test_tempdir("djinn-lsp-which-");
        make_fake_binary(tmp.path(), "ls", "#!/bin/sh\n");
        let result = which_in_path("ls", &tmp.path().to_string_lossy());
        assert_eq!(result, Some(tmp.path().join("ls")));
    }

    #[test]
    fn which_in_path_returns_none_for_missing() {
        let result = which_in_path("definitely_not_a_real_binary_xyz", "/usr/bin");
        assert!(result.is_none());
    }

    #[test]
    fn which_in_path_scans_multiple_dirs() {
        let tmp = crate::test_helpers::test_tempdir("djinn-lsp-which-");
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        make_fake_binary(&second, "ls", "#!/bin/sh\n");
        let path = format!("{}:{}", first.display(), second.display());
        let result = which_in_path("ls", &path);
        assert_eq!(result, Some(second.join("ls")));
    }

    // --- resolve_binary_inner ---

    /// Create a fake executable script in a temp dir.
    fn make_fake_binary(dir: &Path, name: &str, script: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    fn tempdir_in_tmp() -> tempfile::TempDir {
        crate::test_helpers::test_tempdir("djinn-lsp-")
    }

    #[test]
    fn resolve_binary_finds_existing_on_path() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        // Put a fake typescript-language-server on PATH
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();
        make_fake_binary(&path_dir, "typescript-language-server", "#!/bin/sh\n");

        let server = server_for_path(Path::new("foo.ts")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), path_dir.join("typescript-language-server"));
    }

    #[test]
    fn resolve_binary_finds_existing_in_bin_dir() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        make_fake_binary(&bin_dir, "rust-analyzer", "#!/bin/sh\n");

        let server = server_for_path(Path::new("foo.rs")).unwrap();
        // Empty system PATH — binary only in bin_dir
        let result = resolve_binary_inner(&server, &bin_dir, "");

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), bin_dir.join("rust-analyzer"));
    }

    #[test]
    fn resolve_binary_npm_not_found_errors() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");

        let server = server_for_path(Path::new("foo.ts")).unwrap();
        // Empty PATH — no npm, no typescript-language-server
        let result = resolve_binary_inner(&server, &bin_dir, "");

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("npm not found"));
    }

    #[test]
    fn resolve_binary_go_not_found_errors() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");

        let server = server_for_path(Path::new("foo.go")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, "");

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("go not found"));
    }

    #[test]
    fn resolve_binary_rust_no_rustup_errors_with_help() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");

        let server = server_for_path(Path::new("foo.rs")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, "");

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("rust-analyzer not found"), "got: {err}");
        assert!(err.contains("rustup component add"), "got: {err}");
    }

    #[test]
    fn resolve_binary_npm_installs_successfully() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        // Create a fake npm that "installs" by creating the binary in bin_dir/bin/
        let install_target = bin_dir.join("bin");
        let script = format!(
            "#!/bin/sh\nmkdir -p '{}'\ntouch '{}/typescript-language-server'\nchmod +x '{}/typescript-language-server'\n",
            install_target.display(),
            install_target.display(),
            install_target.display(),
        );
        make_fake_binary(&path_dir, "npm", &script);

        let server = server_for_path(Path::new("foo.ts")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert!(result.unwrap().ends_with("typescript-language-server"));
    }

    #[test]
    fn resolve_binary_npm_failure_returns_error() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        // Fake npm that exits with error
        make_fake_binary(&path_dir, "npm", "#!/bin/sh\necho 'ERR!' >&2\nexit 1\n");

        let server = server_for_path(Path::new("foo.ts")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("npm install failed"));
    }

    #[test]
    fn resolve_binary_go_installs_successfully() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        // Fake go that "installs" gopls into GOBIN (which is bin_dir)
        let script = format!(
            "#!/bin/sh\nmkdir -p '{}'\ntouch '{}/gopls'\nchmod +x '{}/gopls'\n",
            bin_dir.display(),
            bin_dir.display(),
            bin_dir.display(),
        );
        make_fake_binary(&path_dir, "go", &script);

        let server = server_for_path(Path::new("foo.go")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert!(result.unwrap().ends_with("gopls"));
    }

    #[test]
    fn resolve_binary_rustup_installs_successfully() {
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        // Fake rustup that "installs" rust-analyzer onto PATH
        let rustup_bin_dir = tmp.path().join("rustup_bin");
        std::fs::create_dir_all(&rustup_bin_dir).unwrap();
        let script = format!(
            "#!/bin/sh\ntouch '{}/rust-analyzer'\nchmod +x '{}/rust-analyzer'\n",
            rustup_bin_dir.display(),
            rustup_bin_dir.display(),
        );
        make_fake_binary(&path_dir, "rustup", &script);

        let server = server_for_path(Path::new("foo.rs")).unwrap();
        // Include rustup_bin_dir in system PATH so the binary is found post-install
        let sys_path = format!("{}:{}", path_dir.display(), rustup_bin_dir.display());
        let result = resolve_binary_inner(&server, &bin_dir, &sys_path);

        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert!(result.unwrap().ends_with("rust-analyzer"));
    }

    #[test]
    fn resolve_binary_rustup_which_fallback_when_not_on_path() {
        // Simulates system-packaged rustup (e.g. Arch) where `component add`
        // succeeds but the binary doesn't appear on PATH.  The code should
        // fall back to `rustup which rust-analyzer` to locate the toolchain binary.
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        // The toolchain binary lives here — NOT on PATH
        let toolchain_bin = tmp.path().join("toolchain_bin");
        std::fs::create_dir_all(&toolchain_bin).unwrap();
        make_fake_binary(&toolchain_bin, "rust-analyzer", "#!/bin/sh\n");

        // Fake rustup: `component add` succeeds (no-op), `which` prints the
        // toolchain path.  Note: the binary does NOT get placed on PATH.
        let ra_path = toolchain_bin.join("rust-analyzer");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"which\" ]; then echo '{}'; else true; fi\n",
            ra_path.display(),
        );
        make_fake_binary(&path_dir, "rustup", &script);

        let server = server_for_path(Path::new("foo.rs")).unwrap();
        // System PATH only has the dir with our fake rustup — no rust-analyzer
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert_eq!(result.unwrap(), ra_path);
    }

    #[test]
    fn resolve_binary_rustup_which_finds_existing_without_install() {
        // When rust-analyzer is already installed via system rustup, `rustup which`
        // should find it during the initial lookup — no `component add` needed.
        let tmp = tempdir_in_tmp();
        let bin_dir = tmp.path().join("djinn_bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        let toolchain_bin = tmp.path().join("toolchain_bin");
        std::fs::create_dir_all(&toolchain_bin).unwrap();
        let ra_path = make_fake_binary(&toolchain_bin, "rust-analyzer", "#!/bin/sh\n");

        // Fake rustup that only handles `which`
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"which\" ]; then echo '{}'; else exit 1; fi\n",
            ra_path.display(),
        );
        make_fake_binary(&path_dir, "rustup", &script);

        let server = server_for_path(Path::new("foo.rs")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert_eq!(result.unwrap(), ra_path);
    }

    // --- symbols formatting and filtering ---

    #[test]
    fn parse_symbol_kind_filter_supports_aliases() {
        let kinds = parse_symbol_kind_filter("function,method,type_parameter").unwrap();
        assert!(kinds.contains(&12));
        assert!(kinds.contains(&6));
        assert!(kinds.contains(&26));
    }

    #[test]
    fn parse_symbol_tree_filters_and_formats_grouped_output() {
        let symbols = vec![json!({
            "name": "Config",
            "kind": 23,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 10, "character": 0 } },
            "children": [
                {
                    "name": "new",
                    "kind": 12,
                    "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 3 } }
                },
                {
                    "name": "value",
                    "kind": 8,
                    "range": { "start": { "line": 2, "character": 0 }, "end": { "line": 2, "character": 5 } }
                }
            ]
        })];

        let entries = parse_symbol_tree(
            &symbols,
            &SymbolQuery {
                depth: Some(1),
                kinds: Some(HashSet::from([23])),
                name_filter: Some("conf".to_string()),
            },
        );

        let output = format_symbol_entries(&entries);
        assert!(output.contains("Struct (1)"));
        assert!(output.contains("- Config (line 1)"));
        assert!(!output.contains("Field"));
        assert!(!output.contains("new"));
    }

    #[test]
    fn parse_symbol_tree_matches_nested_name_paths() {
        let symbols = vec![json!({
            "name": "Outer",
            "kind": 5,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 5, "character": 0 } },
            "children": [
                {
                    "name": "target_method",
                    "kind": 6,
                    "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 5 } }
                }
            ]
        })];

        let entries = parse_symbol_tree(
            &symbols,
            &SymbolQuery {
                depth: None,
                kinds: Some(HashSet::from([6])),
                name_filter: Some("outer/target".to_string()),
            },
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name_path, "Outer/target_method");
    }

    #[test]
    fn format_symbol_entries_falls_back_for_large_outputs() {
        let entries: Vec<SymbolEntry> = (0..250)
            .map(|index| SymbolEntry {
                kind_name: "Function",
                name_path: format!("module/very_long_symbol_name_{index:03}_{}", "x".repeat(40)),
                line: Some(index + 1),
                location: None,
                child_count: 3,
            })
            .collect();

        let output = format_symbol_entries(&entries);
        assert!(output.contains("output shortened"));
    }
}
