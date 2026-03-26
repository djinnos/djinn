use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};

use super::diagnostics::{Diagnostic, DiagnosticsMap, new_diagnostics_map, publish};
use super::{INIT_TIMEOUT, ServerDef, djinn_bin_dir, language_id_for_path, resolve_binary};

pub type PendingResponses =
    Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<serde_json::Value>>>>;
pub type OpenedFiles = Arc<Mutex<HashMap<String, i32>>>;
pub type ClientStdin = Arc<Mutex<ChildStdin>>;

pub struct LspClient {
    pub stdin: ClientStdin,
    pub pid: u32,
    pub reader_handle: tokio::task::JoinHandle<()>,
    pub diagnostics: DiagnosticsMap,
    pub pending: PendingResponses,
    pub seq: Arc<AtomicU64>,
    pub opened: OpenedFiles,
}

pub fn clone_client_refs(c: &LspClient) -> (ClientStdin, DiagnosticsMap, OpenedFiles) {
    (c.stdin.clone(), c.diagnostics.clone(), c.opened.clone())
}

pub fn clone_client_request_refs(c: &LspClient) -> (ClientStdin, PendingResponses, Arc<AtomicU64>) {
    (c.stdin.clone(), c.pending.clone(), c.seq.clone())
}

pub fn kill_client(client: LspClient) {
    client.reader_handle.abort();
    unsafe {
        libc::kill(client.pid as libc::pid_t, libc::SIGTERM);
    }
}

pub async fn spawn_client(server: &ServerDef, root: &Path) -> Result<LspClient, String> {
    let binary_path = resolve_binary(server).await?;
    tracing::info!(server = server.id, binary = %binary_path.display(), "lsp: spawning server");

    let mut cmd = Command::new(&binary_path);
    for arg in server.cmd.iter().skip(1) {
        cmd.arg(arg);
    }

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

    let mut child = cmd.spawn().map_err(|e| {
        tracing::error!(server = server.id, binary = %binary_path.display(), error = %e, "lsp: spawn failed");
        format!("failed to spawn {}: {e}", server.id)
    })?;

    let pid = child
        .id()
        .ok_or_else(|| "could not get child PID".to_string())?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "missing stdin".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "missing stdout".to_string())?;

    let diagnostics = new_diagnostics_map();
    let diagnostics_reader = diagnostics.clone();

    let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));
    let pending_reader = pending.clone();

    let reader_handle = tokio::spawn(async move {
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
                            let error_count = ds
                                .iter()
                                .filter(|d| d.get("severity").and_then(|s| s.as_u64()) == Some(1))
                                .count();
                            tracing::info!(
                                uri = %uri,
                                total = ds.len(),
                                errors = error_count,
                                "lsp: received publishDiagnostics"
                            );
                            let mut out = Vec::new();
                            for d in ds {
                                let sev =
                                    d.get("severity").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                                let msg = d
                                    .get("message")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let diag_line =
                                    d.get("range")
                                        .and_then(|r| r.get("start"))
                                        .and_then(|s| s.get("line"))
                                        .and_then(|x| x.as_u64())
                                        .unwrap_or(0) as u32
                                        + 1;
                                let character =
                                    d.get("range")
                                        .and_then(|r| r.get("start"))
                                        .and_then(|s| s.get("character"))
                                        .and_then(|x| x.as_u64())
                                        .unwrap_or(0) as u32
                                        + 1;
                                out.push(Diagnostic {
                                    file: uri.clone(),
                                    line: diag_line,
                                    character,
                                    severity: sev,
                                    message: msg,
                                });
                            }
                            publish(&diagnostics_reader, uri, out).await;
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
        INIT_TIMEOUT,
    )
    .await
    .map_err(|e| format!("LSP initialize failed for {}: {e}", server.id))?;

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
        pid,
        reader_handle,
        diagnostics,
        pending,
        seq,
        opened: Arc::new(Mutex::new(HashMap::new())),
    })
}

pub async fn write_lsp_message(
    stdin: &Arc<Mutex<ChildStdin>>,
    payload: &str,
) -> Result<(), String> {
    let mut guard = stdin.lock().await;
    let message = format!("Content-Length: {}\r\n\r\n{}", payload.len(), payload);
    guard
        .write_all(message.as_bytes())
        .await
        .map_err(|e| format!("lsp write failed: {e}"))
}

pub async fn send_request(
    stdin: &ClientStdin,
    pending: &PendingResponses,
    seq: &AtomicU64,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
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

    match tokio::time::timeout(timeout, rx).await {
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
            Err(format!("LSP request timed out ({}s)", timeout.as_secs()))
        }
    }
}

pub async fn ensure_did_open(
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
        sleep(Duration::from_millis(100)).await;
    }
    Ok(uri)
}
