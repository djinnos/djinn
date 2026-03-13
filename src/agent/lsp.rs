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

struct LspClient {
    stdin: Arc<Mutex<ChildStdin>>,
    diagnostics: Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>,
    pending: PendingResponses,
    seq: Arc<AtomicU64>,
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
            return;
        };

        let Some(root) = find_root(path, worktree, server.root_markers) else {
            return;
        };

        let key = format!("{}::{}", server.id, root.display());

        {
            let mut inner = self.inner.lock().await;
            if inner.broken_servers.contains(&key) {
                return;
            }
            if !inner.clients.contains_key(&key) {
                match spawn_client(&server, &root).await {
                    Ok(client) => {
                        inner.clients.insert(key.clone(), client);
                    }
                    Err(_) => {
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
        let Some((stdin, diagnostics)) = client else {
            return;
        };

        let uri = format!("file://{}", path.display());
        let text = match tokio::fs::read_to_string(path).await {
            Ok(v) => v,
            Err(_) => return,
        };

        let open = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id_for_path(path).unwrap_or("plaintext"),
                    "version": 1,
                    "text": text,
                }
            }
        });

        let _ = write_lsp_message(&stdin, &open.to_string()).await;

        if wait_for_diagnostics {
            let deadline = Instant::now() + Duration::from_secs(3);
            let debounce = Duration::from_millis(150);
            let mut last_change = Instant::now();
            let mut prev_snapshot: Option<usize> = None;

            loop {
                let now = Instant::now();
                if now >= deadline {
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
                        prev_snapshot = Some(len);
                        last_change = Instant::now();
                    }
                    // Count changed — reset debounce
                    (Some(prev), Some(len)) if prev != len => {
                        prev_snapshot = Some(len);
                        last_change = Instant::now();
                    }
                    // Diagnostics present but unchanged — check debounce expiry
                    (Some(_), Some(_)) => {
                        if now.duration_since(last_change) >= debounce {
                            break;
                        }
                    }
                    // Diagnostics cleared (shouldn't happen normally)
                    (Some(_), None) => {
                        prev_snapshot = None;
                        last_change = Instant::now();
                    }
                }

                sleep(Duration::from_millis(25)).await;
            }
        }
    }

    pub async fn diagnostics(&self) -> Vec<Diagnostic> {
        let clients = {
            let inner = self.inner.lock().await;
            inner
                .clients
                .values()
                .map(|c| c.diagnostics.clone())
                .collect::<Vec<_>>()
        };

        let mut out = Vec::new();
        for d in clients {
            let map = d.lock().await;
            for values in map.values() {
                out.extend(values.clone());
            }
        }
        out
    }
}

fn clone_client_refs(c: &LspClient) -> (ClientStdin, DiagnosticsMap) {
    (c.stdin.clone(), c.diagnostics.clone())
}

fn clone_client_request_refs(
    c: &LspClient,
) -> (ClientStdin, PendingResponses, Arc<AtomicU64>) {
    (c.stdin.clone(), c.pending.clone(), c.seq.clone())
}

struct ServerDef {
    id: &'static str,
    cmd: &'static [&'static str],
    root_markers: &'static [&'static str],
}

fn server_for_path(path: &Path) -> Option<ServerDef> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => Some(ServerDef {
            id: "rust-analyzer",
            cmd: &["rust-analyzer"],
            root_markers: &["Cargo.toml"],
        }),
        Some("go") => Some(ServerDef {
            id: "gopls",
            cmd: &["gopls"],
            root_markers: &["go.mod"],
        }),
        Some("ts") | Some("tsx") | Some("js") | Some("jsx") => Some(ServerDef {
            id: "typescript-language-server",
            cmd: &["typescript-language-server", "--stdio"],
            root_markers: &["package.json", "tsconfig.json"],
        }),
        Some("py") => Some(ServerDef {
            id: "pyright",
            cmd: &["pyright-langserver", "--stdio"],
            root_markers: &["pyproject.toml", "setup.py"],
        }),
        _ => None,
    }
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
    let mut cmd = Command::new(server.cmd[0]);
    for arg in server.cmd.iter().skip(1) {
        cmd.arg(arg);
    }
    cmd.current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {}: {e}", server.id))?;

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

                        if v.get("method").and_then(|m| m.as_str())
                            == Some("textDocument/publishDiagnostics")
                        {
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
    let init = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": null,
            "rootUri": format!("file://{}", root.display()),
            "capabilities": {}
        }
    });
    write_lsp_message(&stdin, &init.to_string()).await?;
    let inited = json!({"jsonrpc":"2.0","method":"initialized","params":{}});
    write_lsp_message(&stdin, &inited.to_string()).await?;

    Ok(LspClient {
        stdin,
        diagnostics,
        pending,
        seq: Arc::new(AtomicU64::new(2)),
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
async fn ensure_did_open(stdin: &ClientStdin, path: &Path) -> Result<String, String> {
    let uri = format!("file://{}", path.display());
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let open = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": uri,
                "languageId": language_id_for_path(path).unwrap_or("plaintext"),
                "version": 1,
                "text": text,
            }
        }
    });
    write_lsp_message(stdin, &open.to_string()).await?;
    // Give the server a moment to index after open
    sleep(Duration::from_millis(100)).await;
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
    ) -> Result<(ClientStdin, PendingResponses, Arc<AtomicU64>), String> {
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
        Ok(clone_client_request_refs(client))
    }

    /// Send textDocument/hover and return the hover contents as text.
    pub async fn hover(
        &self,
        worktree: &Path,
        path: &Path,
        line: u32,
        character: u32,
    ) -> Result<String, String> {
        let (stdin, pending, seq) = self.get_request_refs(worktree, path).await?;
        let uri = ensure_did_open(&stdin, path).await?;

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
        let (stdin, pending, seq) = self.get_request_refs(worktree, path).await?;
        let uri = ensure_did_open(&stdin, path).await?;

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
        let (stdin, pending, seq) = self.get_request_refs(worktree, path).await?;
        let uri = ensure_did_open(&stdin, path).await?;

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
        let (stdin, pending, seq) = self.get_request_refs(worktree, path).await?;
        let uri = ensure_did_open(&stdin, path).await?;

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
