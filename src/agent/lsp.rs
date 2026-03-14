use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, sleep};

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub file: String,
    pub line: u32,
    pub character: u32,
    pub severity: u32,
    pub message: String,
}

#[derive(Clone)]
pub struct LspManager {
    inner: Arc<Mutex<LspInner>>,
}

struct LspInner {
    clients: HashMap<String, LspClient>,
    broken_servers: std::collections::HashSet<String>,
}

/// Pending response channels keyed by JSON-RPC request id.
type PendingResponses =
    Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<serde_json::Value>>>>;

/// Tracks per-file open state: version counter for didChange notifications.
type OpenedFiles = Arc<Mutex<HashMap<String, i32>>>;

struct LspClient {
    stdin: Arc<Mutex<ChildStdin>>,
    diagnostics: Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>,
    pending: PendingResponses,
    seq: Arc<AtomicU64>,
    opened: OpenedFiles,
}

type ClientStdin = Arc<Mutex<ChildStdin>>;
type DiagnosticsMap = Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>;

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
            })),
        }
    }

    #[allow(dead_code)]
    pub async fn shutdown_all(&self) {
        let mut inner = self.inner.lock().await;
        inner.clients.clear();
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
            diagnostics.lock().await.remove(&uri);

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
            diagnostics.lock().await.remove(&uri);

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

    pub async fn diagnostics(&self) -> Vec<Diagnostic> {
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

        let mut out = Vec::new();
        for d in clients {
            let map = d.lock().await;
            for values in map.values() {
                out.extend(values.clone());
            }
        }
        let errors = out.iter().filter(|d| d.severity == 1).count();
        tracing::info!(
            clients = client_count,
            total = out.len(),
            errors = errors,
            "lsp: diagnostics() called"
        );
        out
    }
}

fn clone_client_refs(c: &LspClient) -> (ClientStdin, DiagnosticsMap, OpenedFiles) {
    (c.stdin.clone(), c.diagnostics.clone(), c.opened.clone())
}

fn clone_client_request_refs(
    c: &LspClient,
) -> (ClientStdin, PendingResponses, Arc<AtomicU64>) {
    (c.stdin.clone(), c.pending.clone(), c.seq.clone())
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
    /// Install via `rustup component add` or `cargo install` fallback.
    RustComponent(&'static str),
    /// Install via `go install <pkg>@latest` with GOBIN=~/.djinn/bin
    GoInstall(&'static str),
}

fn server_for_path(path: &Path) -> Option<ServerDef> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some(ServerDef {
            id: "rust-analyzer",
            cmd: &["rust-analyzer"],
            root_markers: &["Cargo.toml"],
            install: InstallMethod::RustComponent("rust-analyzer"),
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
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share"))
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
            let output = cmd.output().map_err(|e| format!("npm install failed: {e}"))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(format!("npm install failed: {stderr}"));
            }
        }
        InstallMethod::RustComponent(component) => {
            if let Some(rustup) = which_in_path("rustup", system_path) {
                tracing::info!(component = component, "lsp: running rustup component add");
                let output = std::process::Command::new(rustup)
                    .args(["component", "add", component])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .map_err(|e| format!("rustup failed: {e}"))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!("rustup component add failed: {stderr}"));
                }
            } else if let Some(cargo) = which_in_path("cargo", system_path) {
                tracing::info!(component = component, "lsp: running cargo install (no rustup found)");
                let output = std::process::Command::new(cargo)
                    .args(["install", component])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .output()
                    .map_err(|e| format!("cargo install failed: {e}"))?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(format!("cargo install {component} failed: {stderr}"));
                }
            } else {
                return Err(format!("neither rustup nor cargo found — cannot install {component}"));
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

async fn spawn_client(server: &ServerDef, root: &Path) -> Result<LspClient, String> {
    let binary_path = resolve_binary(server).await?;
    tracing::info!(server = server.id, binary = %binary_path.display(), "lsp: spawning server");

    let mut cmd = Command::new(&binary_path);
    for arg in server.cmd.iter().skip(1) {
        cmd.arg(arg);
    }

    // Ensure our managed bin dir is on PATH for the LSP process too
    // (e.g. typescript-language-server needs to find tsserver).
    let bin_dir = djinn_bin_dir();
    let system_path = std::env::var("PATH").unwrap_or_default();
    let augmented_path = format!(
        "{}:{}:{}",
        bin_dir.display(),
        bin_dir.join("bin").display(),
        system_path
    );

    cmd.current_dir(root)
        .env("PATH", &augmented_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| {
            tracing::error!(server = server.id, binary = %binary_path.display(), error = %e, "lsp: spawn failed");
            format!("failed to spawn {}: {e}", server.id)
        })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "missing stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "missing stdout".to_string())?;

    let diagnostics: Arc<Mutex<HashMap<String, Vec<Diagnostic>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let diagnostics_reader = diagnostics.clone();

    let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));
    let pending_reader = pending.clone();

    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut content_length: usize = 0;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    if line == "\r\n" {
                        if content_length == 0 {
                            continue;
                        }
                        let mut buf = vec![0u8; content_length];
                        if reader.read_exact(&mut buf).await.is_err() {
                            break;
                        }
                        content_length = 0;
                        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf) else {
                            continue;
                        };

                        // Route responses (messages with an id and result/error) to
                        // pending request channels.
                        if let Some(id) = v.get("id").and_then(|i| i.as_u64())
                            && (v.get("result").is_some() || v.get("error").is_some())
                        {
                            let sender = pending_reader.lock().await.remove(&id);
                            if let Some(tx) = sender {
                                let _ = tx.send(v);
                            }
                            continue;
                        }

                        let method = v.get("method").and_then(|m| m.as_str());
                        if method == Some("textDocument/publishDiagnostics") {
                            let params = v.get("params").cloned().unwrap_or_default();
                            let uri = params
                                .get("uri")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string();
                            let ds = params
                                .get("diagnostics")
                                .and_then(|x| x.as_array())
                                .cloned()
                                .unwrap_or_default();
                            let error_count = ds.iter().filter(|d| {
                                d.get("severity").and_then(|s| s.as_u64()) == Some(1)
                            }).count();
                            tracing::info!(
                                uri = %uri,
                                total = ds.len(),
                                errors = error_count,
                                "lsp: received publishDiagnostics"
                            );
                            let mut out = Vec::new();
                            for d in ds {
                                let sev = d
                                    .get("severity")
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0)
                                    as u32;
                                let msg = d
                                    .get("message")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let diag_line = d
                                    .get("range")
                                    .and_then(|r| r.get("start"))
                                    .and_then(|s| s.get("line"))
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0)
                                    as u32
                                    + 1;
                                let character = d
                                    .get("range")
                                    .and_then(|r| r.get("start"))
                                    .and_then(|s| s.get("character"))
                                    .and_then(|x| x.as_u64())
                                    .unwrap_or(0)
                                    as u32
                                    + 1;
                                out.push(Diagnostic {
                                    file: uri.clone(),
                                    line: diag_line,
                                    character,
                                    severity: sev,
                                    message: msg,
                                });
                            }
                            diagnostics_reader.lock().await.insert(uri, out);
                        } else if let Some(m) = method {
                            tracing::debug!(method = m, "lsp: received server notification");
                        }
                    } else if let Some(v) = line.strip_prefix("Content-Length:") {
                        content_length = v.trim().parse::<usize>().unwrap_or(0);
                    }
                }
                Err(_) => break,
            }
        }
    });

    let stdin = Arc::new(Mutex::new(stdin));
    let seq = Arc::new(AtomicU64::new(2));

    // Send initialize and wait for the response — the LSP spec requires the
    // client to wait before sending `initialized` or any other notification.
    let init_result = send_request(
        &stdin,
        &pending,
        &seq,
        "initialize",
        json!({
            "processId": null,
            "rootUri": format!("file://{}", root.display()),
            "capabilities": {
                "textDocument": {
                    "synchronization": {
                        "didOpen": true,
                        "didChange": true,
                        "willSave": false,
                        "didSave": true,
                    },
                    "publishDiagnostics": {
                        "versionSupport": true,
                    },
                },
            },
        }),
    )
    .await
    .map_err(|e| format!("LSP initialize failed for {}: {e}", server.id))?;

    // Log server name for debugging
    if let Some(name) = init_result
        .get("serverInfo")
        .and_then(|s| s.get("name"))
        .and_then(|n| n.as_str())
    {
        tracing::debug!("LSP server initialized: {name}");
    }

    let inited = json!({"jsonrpc":"2.0","method":"initialized","params":{}});
    write_lsp_message(&stdin, &inited.to_string()).await?;

    Ok(LspClient {
        stdin,
        diagnostics,
        pending,
        seq,
        opened: Arc::new(Mutex::new(HashMap::new())),
    })
}

async fn write_lsp_message(stdin: &Arc<Mutex<ChildStdin>>, payload: &str) -> Result<(), String> {
    let mut guard = stdin.lock().await;
    let message = format!("Content-Length: {}\r\n\r\n{}", payload.len(), payload);
    guard
        .write_all(message.as_bytes())
        .await
        .map_err(|e| format!("lsp write failed: {e}"))
}

/// Send a JSON-RPC request and wait for the response (up to 10s).
async fn send_request(
    stdin: &ClientStdin,
    pending: &PendingResponses,
    seq: &AtomicU64,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let id = seq.fetch_add(1, Ordering::SeqCst);
    let msg = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });

    let (tx, rx) = tokio::sync::oneshot::channel();
    pending.lock().await.insert(id, tx);

    write_lsp_message(stdin, &msg.to_string()).await?;

    match tokio::time::timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(v)) => {
            if let Some(err) = v.get("error") {
                Err(format!("LSP error: {err}"))
            } else {
                Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
            }
        }
        Ok(Err(_)) => Err("LSP response channel closed".to_string()),
        Err(_) => {
            pending.lock().await.remove(&id);
            Err("LSP request timed out (10s)".to_string())
        }
    }
}

/// Ensure the file is opened in the LSP server so queries work.
async fn ensure_did_open(
    stdin: &ClientStdin,
    path: &Path,
    opened: &OpenedFiles,
) -> Result<String, String> {
    let uri = format!("file://{}", path.display());
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;

    let mut opened_guard = opened.lock().await;
    if opened_guard.contains_key(&uri) {
        // Already opened — no need to re-open for query purposes.
        drop(opened_guard);
    } else {
        opened_guard.insert(uri.clone(), 0);
        drop(opened_guard);

        let open = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id_for_path(path).unwrap_or("plaintext"),
                    "version": 0,
                    "text": text,
                }
            }
        });
        write_lsp_message(stdin, &open.to_string()).await?;
        // Give the server a moment to index after open
        sleep(Duration::from_millis(100)).await;
    }
    Ok(uri)
}

/// Format an LSP Location or LocationLink into a human-readable string.
fn format_location(loc: &serde_json::Value) -> String {
    let uri = loc
        .get("uri")
        .or_else(|| loc.get("targetUri"))
        .and_then(|u| u.as_str())
        .unwrap_or("?");
    let range = loc
        .get("range")
        .or_else(|| loc.get("targetSelectionRange"));
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

/// Format LSP DocumentSymbol or SymbolInformation into a readable list.
fn format_symbol(sym: &serde_json::Value, indent: usize) -> String {
    let name = sym.get("name").and_then(|n| n.as_str()).unwrap_or("?");
    let kind_num = sym.get("kind").and_then(|k| k.as_u64()).unwrap_or(0);
    let kind = symbol_kind_name(kind_num);
    let prefix = "  ".repeat(indent);

    let mut result = format!("{prefix}{kind} {name}");

    // If it has a location (SymbolInformation style), show it
    if let Some(loc) = sym.get("location") {
        result.push_str(&format!("  → {}", format_location(loc)));
    } else if let Some(range) = sym.get("range") {
        let line = range
            .get("start")
            .and_then(|s| s.get("line"))
            .and_then(|l| l.as_u64())
            .unwrap_or(0)
            + 1;
        result.push_str(&format!("  [line {line}]"));
    }

    // Recurse into children (DocumentSymbol style)
    if let Some(children) = sym.get("children").and_then(|c| c.as_array()) {
        for child in children {
            result.push('\n');
            result.push_str(&format_symbol(child, indent + 1));
        }
    }

    result
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
        )
        .await?;

        if result.is_null() {
            return Ok("No symbols found in this document.".to_string());
        }

        let symbols = result.as_array().cloned().unwrap_or_default();
        if symbols.is_empty() {
            return Ok("No symbols found in this document.".to_string());
        }

        let formatted: Vec<String> = symbols.iter().map(|s| format_symbol(s, 0)).collect();
        Ok(formatted.join("\n"))
    }
}

pub fn format_diagnostics_xml(diags: Vec<Diagnostic>) -> String {
    let mut by_file: HashMap<String, Vec<Diagnostic>> = HashMap::new();
    for d in diags.into_iter().filter(|d| d.severity == 1) {
        by_file.entry(d.file.clone()).or_default().push(d);
    }

    let mut files: Vec<_> = by_file.into_iter().collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files.truncate(5);

    let mut out = String::new();
    for (file, mut items) in files {
        items.truncate(20);
        out.push_str(&format!("<diagnostics file=\"{}\">\n", file));
        for d in items {
            out.push_str(&format!(
                "ERROR [{}:{}] {}\n",
                d.line, d.character, d.message
            ));
        }
        out.push_str("</diagnostics>\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert_eq!(
            language_id_for_path(Path::new("a.js")),
            Some("javascript")
        );
        assert_eq!(
            language_id_for_path(Path::new("a.jsx")),
            Some("javascriptreact")
        );
        assert_eq!(language_id_for_path(Path::new("a.json")), Some("json"));
        assert_eq!(language_id_for_path(Path::new("a.toml")), Some("toml"));
        assert_eq!(language_id_for_path(Path::new("a.yaml")), Some("yaml"));
        assert_eq!(language_id_for_path(Path::new("a.yml")), Some("yaml"));
        assert_eq!(
            language_id_for_path(Path::new("a.md")),
            Some("markdown")
        );
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

    #[tokio::test]
    async fn lsp_manager_diagnostics_empty_by_default() {
        let mgr = LspManager::new();
        assert!(mgr.diagnostics().await.is_empty());
    }

    #[tokio::test]
    async fn lsp_manager_touch_file_no_server_for_txt() {
        let mgr = LspManager::new();
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();
        // Should return without error even though no server matches
        mgr.touch_file(tmp.path(), &file, false).await;
        assert!(mgr.diagnostics().await.is_empty());
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
        diagnostics.lock().await.insert(
            uri.clone(),
            vec![make_diag(&uri, 1, 1, 1, "old error")],
        );
        assert_eq!(diagnostics.lock().await.get(&uri).unwrap().len(), 1);

        // Simulate clearing before re-touch (what touch_file now does)
        diagnostics.lock().await.remove(&uri);
        assert!(diagnostics.lock().await.get(&uri).is_none());
    }

    // --- which_in_path ---

    #[test]
    fn which_in_path_finds_existing_binary() {
        // /usr/bin/ls should exist on any Linux
        let result = which_in_path("ls", "/usr/bin");
        assert_eq!(result, Some(PathBuf::from("/usr/bin/ls")));
    }

    #[test]
    fn which_in_path_returns_none_for_missing() {
        let result = which_in_path("definitely_not_a_real_binary_xyz", "/usr/bin");
        assert!(result.is_none());
    }

    #[test]
    fn which_in_path_scans_multiple_dirs() {
        let result = which_in_path("ls", "/nonexistent:/usr/bin:/also_fake");
        assert_eq!(result, Some(PathBuf::from("/usr/bin/ls")));
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

    #[test]
    fn resolve_binary_finds_existing_on_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_dir = tmp.path().join("djinn_bin");
        std::fs::create_dir_all(&bin_dir).unwrap();

        // Put a fake typescript-language-server on PATH
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();
        make_fake_binary(&path_dir, "typescript-language-server", "#!/bin/sh\n");

        let server = server_for_path(Path::new("foo.ts")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, &path_dir.to_string_lossy());

        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            path_dir.join("typescript-language-server")
        );
    }

    #[test]
    fn resolve_binary_finds_existing_in_bin_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
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
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_dir = tmp.path().join("djinn_bin");

        let server = server_for_path(Path::new("foo.ts")).unwrap();
        // Empty PATH — no npm, no typescript-language-server
        let result = resolve_binary_inner(&server, &bin_dir, "");

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("npm not found"));
    }

    #[test]
    fn resolve_binary_go_not_found_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_dir = tmp.path().join("djinn_bin");

        let server = server_for_path(Path::new("foo.go")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, "");

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("go not found"));
    }

    #[test]
    fn resolve_binary_rust_no_rustup_no_cargo_errors() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_dir = tmp.path().join("djinn_bin");

        let server = server_for_path(Path::new("foo.rs")).unwrap();
        let result = resolve_binary_inner(&server, &bin_dir, "");

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("neither rustup nor cargo found"));
    }

    #[test]
    fn resolve_binary_npm_installs_successfully() {
        let tmp = tempfile::TempDir::new().unwrap();
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
        let tmp = tempfile::TempDir::new().unwrap();
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
        let tmp = tempfile::TempDir::new().unwrap();
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
    fn resolve_binary_cargo_fallback_installs_successfully() {
        let tmp = tempfile::TempDir::new().unwrap();
        let bin_dir = tmp.path().join("djinn_bin");
        let path_dir = tmp.path().join("fakepath");
        std::fs::create_dir_all(&path_dir).unwrap();

        // No rustup, but a fake cargo that "installs" rust-analyzer into
        // ~/.cargo/bin (simulated via PATH dir)
        let cargo_bin = tmp.path().join("cargo_bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        let script = format!(
            "#!/bin/sh\ntouch '{}/rust-analyzer'\nchmod +x '{}/rust-analyzer'\n",
            cargo_bin.display(),
            cargo_bin.display(),
        );
        make_fake_binary(&path_dir, "cargo", &script);

        let server = server_for_path(Path::new("foo.rs")).unwrap();
        // Include cargo_bin in system PATH so the binary is found post-install
        let sys_path = format!("{}:{}", path_dir.display(), cargo_bin.display());
        let result = resolve_binary_inner(&server, &bin_dir, &sys_path);

        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        assert!(result.unwrap().ends_with("rust-analyzer"));
    }

    // --- format_symbol ---

    #[test]
    fn format_symbol_with_location() {
        let sym = json!({
            "name": "my_func",
            "kind": 12,
            "location": {
                "uri": "file:///foo.rs",
                "range": { "start": { "line": 5, "character": 0 }, "end": { "line": 5, "character": 10 } }
            }
        });
        let result = format_symbol(&sym, 0);
        assert!(result.contains("Function my_func"));
        assert!(result.contains("/foo.rs:6:1"));
    }

    #[test]
    fn format_symbol_with_children() {
        let sym = json!({
            "name": "MyStruct",
            "kind": 23,
            "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 10, "character": 0 } },
            "children": [
                {
                    "name": "field_a",
                    "kind": 8,
                    "range": { "start": { "line": 1, "character": 4 }, "end": { "line": 1, "character": 20 } }
                }
            ]
        });
        let result = format_symbol(&sym, 0);
        assert!(result.contains("Struct MyStruct"));
        assert!(result.contains("  Field field_a"));
    }
}
