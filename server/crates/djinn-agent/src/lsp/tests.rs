use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use super::Diagnostic;
use super::client::LspClient;
use tokio::sync::Mutex;

mod diagnostics;
mod manager_lifecycle;
mod manager_state;

fn make_diag(file: &str, line: u32, character: u32, severity: u32, msg: &str) -> Diagnostic {
    Diagnostic {
        file: file.to_string(),
        line,
        character,
        severity,
        message: msg.to_string(),
    }
}

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
