use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

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

struct LspClient {
    stdin: Arc<Mutex<ChildStdin>>,
    diagnostics: Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>,
    #[allow(dead_code)]
    seq: u64,
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
            loop {
                {
                    let map = diagnostics.lock().await;
                    if map.contains_key(&uri) {
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    break;
                }
                sleep(Duration::from_millis(150)).await;
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
                        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf)
                            && v.get("method").and_then(|m| m.as_str())
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
        seq: 2,
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
