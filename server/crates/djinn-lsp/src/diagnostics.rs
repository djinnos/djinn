use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub file: String,
    pub line: u32,
    pub character: u32,
    pub severity: u32,
    pub message: String,
}

pub type DiagnosticsMap = Arc<Mutex<HashMap<String, Vec<Diagnostic>>>>;

pub fn new_diagnostics_map() -> DiagnosticsMap {
    Arc::new(Mutex::new(HashMap::new()))
}

pub async fn clear_uri(diagnostics: &DiagnosticsMap, uri: &str) {
    diagnostics.lock().await.remove(uri);
}

pub async fn publish(diagnostics: &DiagnosticsMap, uri: String, values: Vec<Diagnostic>) {
    diagnostics.lock().await.insert(uri, values);
}

pub async fn collect_for_worktree(
    diagnostics: &[DiagnosticsMap],
    worktree: &Path,
) -> Vec<Diagnostic> {
    let prefix = format!("file://{}", worktree.display());
    let mut out = Vec::new();

    for map in diagnostics {
        let map = map.lock().await;
        for values in map.values() {
            for diag in values {
                if diag.file.starts_with(&prefix) {
                    out.push(diag.clone());
                }
            }
        }
    }

    out
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
