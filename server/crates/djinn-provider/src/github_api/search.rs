//! GitHub Code Search and file content retrieval.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::github_api::GitHubApiClient;
use crate::github_api::transport::handle_rate_limit;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A single code-search hit from GitHub's `/search/code` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSearchHit {
    pub result_id: usize,
    pub repository: String,
    pub path: String,
    pub language: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_number: Option<u32>,
    pub snippet: String,
    pub url: String,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
}

/// Aggregate result from a code search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSearchResult {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_filter: Option<String>,
    pub total_results: u64,
    pub results_shown: usize,
    pub truncated: bool,
    pub results: Vec<CodeSearchHit>,
}

/// Result of fetching a file's content from GitHub.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileFetchResult {
    pub repository: String,
    pub path: String,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub url: String,
    pub size_bytes: u64,
    pub start_line: u32,
    pub end_line: u32,
    pub truncated: bool,
    pub content: String,
}

// ── Constants ─────────────────────────────────────────────────────────────────

const MAX_RESULTS: usize = 15;
const MAX_SNIPPET_LINES: usize = 12;
/// Maximum file size we'll fetch in full (256 KB).
const MAX_FILE_SIZE: u64 = 256 * 1024;

// ── Implementation ────────────────────────────────────────────────────────────

impl GitHubApiClient {
    /// Search GitHub code using the Code Search API.
    ///
    /// Returns up to `limit` (default 15) results with code snippets via the
    /// `text-match` media type.
    pub async fn search_code(
        &self,
        query: &str,
        language: Option<&str>,
        repo: Option<&str>,
        path: Option<&str>,
        limit: Option<usize>,
    ) -> Result<CodeSearchResult> {
        let query = query.trim();
        if query.is_empty() {
            return Err(anyhow!("query must not be empty"));
        }
        if query.len() > 1000 {
            return Err(anyhow!("query too long (max 1000 chars)"));
        }

        // Build the GitHub code search query string.
        let mut q = query.to_string();
        if let Some(l) = language.filter(|s| !s.trim().is_empty()) {
            q.push_str(&format!(" language:{}", l.trim()));
        }
        if let Some(r) = repo.filter(|s| !s.trim().is_empty()) {
            let r = r.trim();
            if !r.contains('/') || r.matches('/').count() != 1 {
                return Err(anyhow!("repo must be in 'owner/repo' format"));
            }
            q.push_str(&format!(" repo:{r}"));
        }
        if let Some(p) = path.filter(|s| !s.trim().is_empty()) {
            q.push_str(&format!(" path:{}", p.trim()));
        }

        let per_page = limit.unwrap_or(MAX_RESULTS).min(100);
        let encoded_q = urlencoded(&q);
        let url = format!(
            "{}/search/code?q={}&per_page={}",
            self.base_url, encoded_q, per_page
        );

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github.text-match+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        let status = resp.status();
        if status.as_u16() == 422 {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub API rejected query (422): {body}"));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub API returned {} : {}", status, body));
        }

        let body: serde_json::Value = resp.json().await?;
        parse_search_response(
            query,
            language.map(|s| s.trim()),
            repo.map(|s| s.trim()),
            path.map(|s| s.trim()),
            per_page,
            &body,
        )
    }

    /// Fetch the contents of a file from a GitHub repository.
    ///
    /// Supports optional line-range selection via `start_line` / `end_line`
    /// (1-based, inclusive).
    pub async fn fetch_file(
        &self,
        repo: &str,
        path: &str,
        git_ref: Option<&str>,
        start_line: Option<u32>,
        end_line: Option<u32>,
    ) -> Result<FileFetchResult> {
        let repo = repo.trim();
        let path = path.trim().trim_start_matches('/');
        if repo.is_empty() || !repo.contains('/') || repo.matches('/').count() != 1 {
            return Err(anyhow!("repo must be in 'owner/repo' format"));
        }
        if path.is_empty() {
            return Err(anyhow!("path must not be empty"));
        }

        let mut url = format!("{}/repos/{}/contents/{}", self.base_url, repo, path);
        let resolved_ref = git_ref.unwrap_or("HEAD");
        if resolved_ref != "HEAD" {
            url.push_str(&format!("?ref={}", urlencoded(resolved_ref)));
        }

        let resp = self
            .send_with_retry(|token| {
                let url = url.clone();
                let http = self.http.clone();
                async move {
                    let resp = http
                        .get(&url)
                        .bearer_auth(&token)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await?;
                    handle_rate_limit(resp).await
                }
            })
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("fetch_file failed ({}): {}", status, body));
        }

        let body: serde_json::Value = resp.json().await?;

        let size = body.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        let file_url = body
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // For files > MAX_FILE_SIZE, use the git blob API to avoid the
        // contents endpoint's 1MB base64 limit.
        let raw_content = if size > MAX_FILE_SIZE {
            // If no line range given, tell the caller it's too large.
            if start_line.is_none() && end_line.is_none() {
                return Ok(FileFetchResult {
                    repository: repo.to_string(),
                    path: path.to_string(),
                    git_ref: resolved_ref.to_string(),
                    url: file_url,
                    size_bytes: size,
                    start_line: 0,
                    end_line: 0,
                    truncated: true,
                    content: format!(
                        "File is {size} bytes — too large to fetch in full. \
                         Use start_line/end_line to read a section, or narrow your search."
                    ),
                });
            }
            // Still need to fetch the whole thing for line slicing —
            // use the raw download URL if available.
            let download_url = body
                .get("download_url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("no download_url available for large file"))?;

            let resp = self
                .send_with_retry(|token| {
                    let download_url = download_url.to_string();
                    let http = self.http.clone();
                    async move {
                        let resp = http.get(&download_url).bearer_auth(&token).send().await?;
                        Ok(resp)
                    }
                })
                .await?;
            resp.text().await?
        } else {
            // Decode base64 content from the contents API response.
            let encoded = body.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let clean: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&clean)
                .map_err(|e| anyhow!("base64 decode failed: {e}"))?;
            String::from_utf8_lossy(&bytes).into_owned()
        };

        // Apply line-range selection.
        let all_lines: Vec<&str> = raw_content.lines().collect();
        let total_lines = all_lines.len() as u32;
        let start = start_line.unwrap_or(1).max(1);
        let end = end_line.unwrap_or(total_lines).min(total_lines);

        let selected: Vec<&str> = all_lines
            .iter()
            .skip((start - 1) as usize)
            .take((end - start + 1) as usize)
            .copied()
            .collect();
        let truncated = start > 1 || end < total_lines;

        Ok(FileFetchResult {
            repository: repo.to_string(),
            path: path.to_string(),
            git_ref: resolved_ref.to_string(),
            url: file_url,
            size_bytes: size,
            start_line: start,
            end_line: end.min(start + selected.len().saturating_sub(1) as u32),
            truncated,
            content: selected.join("\n"),
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_search_response(
    query: &str,
    language: Option<&str>,
    repo: Option<&str>,
    path_filter: Option<&str>,
    per_page: usize,
    body: &serde_json::Value,
) -> Result<CodeSearchResult> {
    let total_results = body
        .get("total_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let items = body
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut results = Vec::with_capacity(per_page.min(items.len()));

    for (idx, item) in items.iter().take(per_page).enumerate() {
        let repository = item
            .pointer("/repository/full_name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let file_path = item
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let html_url = item
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let score = item.get("score").and_then(|v| v.as_f64());

        let (snippet, first_line) = extract_text_matches(item);
        let lang = language_from_path(&file_path);

        results.push(CodeSearchHit {
            result_id: idx + 1,
            repository,
            path: file_path,
            language: lang.to_string(),
            line_number: first_line,
            snippet,
            url: html_url,
            git_ref: None,
            score,
        });
    }

    Ok(CodeSearchResult {
        query: query.to_string(),
        language: language.map(|s| s.to_string()),
        repo: repo.map(|s| s.to_string()),
        path_filter: path_filter.map(|s| s.to_string()),
        total_results,
        results_shown: results.len(),
        truncated: total_results > results.len() as u64,
        results,
    })
}

/// Extract text-match fragments and the first matching line number.
fn extract_text_matches(item: &serde_json::Value) -> (String, Option<u32>) {
    let text_matches = match item.get("text_matches").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return (String::new(), None),
    };

    let mut fragments: Vec<String> = Vec::new();
    let mut first_line: Option<u32> = None;

    for tm in text_matches {
        if let Some(fragment) = tm.get("fragment").and_then(|v| v.as_str()) {
            let truncated = truncate_snippet(fragment);
            if !truncated.is_empty() {
                fragments.push(truncated);
            }
        }
        // The text_matches don't directly give line numbers, but we can
        // estimate from the fragment content.
        if first_line.is_none()
            && let Some(matches) = tm.get("matches").and_then(|v| v.as_array())
            && let Some(first) = matches.first()
            // indices[0] is the byte offset within the fragment
            && let Some(indices) = first.get("indices").and_then(|v| v.as_array())
            && let Some(start) = indices.first().and_then(|v| v.as_u64())
            // Count newlines before the match to get a rough line number
            && let Some(fragment) = tm.get("fragment").and_then(|v| v.as_str())
        {
            let line = fragment[..start as usize].matches('\n').count() as u32 + 1;
            first_line = Some(line);
        }
    }
    (fragments.join("\n---\n"), first_line)
}

fn truncate_snippet(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_end();
        lines.push(trimmed);
        if lines.len() >= MAX_SNIPPET_LINES {
            break;
        }
    }
    lines.join("\n")
}

fn language_from_path(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "jsx" => "javascript",
        "tsx" => "typescript",
        "java" => "java",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "swift" => "swift",
        "kt" => "kotlin",
        "scala" => "scala",
        "sh" | "bash" | "zsh" => "bash",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "md" | "markdown" => "markdown",
        "lua" => "lua",
        "r" => "r",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "zig" => "zig",
        "nim" => "nim",
        "dart" => "dart",
        _ => "text",
    }
}

/// Percent-encode a string for use in URL query parameters.
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(char::from(b"0123456789ABCDEF"[(b >> 4) as usize]));
                out.push(char::from(b"0123456789ABCDEF"[(b & 0xf) as usize]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_response() {
        let body = serde_json::json!({});
        let result = parse_search_response("test", None, None, None, 15, &body).unwrap();
        assert_eq!(result.total_results, 0);
        assert!(result.results.is_empty());
    }

    #[test]
    fn parse_typical_response() {
        let body = serde_json::json!({
            "total_count": 42,
            "items": [{
                "repository": { "full_name": "tokio-rs/tokio" },
                "path": "tokio/src/runtime/mod.rs",
                "html_url": "https://github.com/tokio-rs/tokio/blob/main/tokio/src/runtime/mod.rs",
                "score": 1.5,
                "text_matches": [{
                    "fragment": "pub fn new() -> Runtime {\n    // create runtime\n}",
                    "matches": [{ "indices": [0, 10] }]
                }]
            }]
        });
        let result = parse_search_response("pub fn new", None, None, None, 15, &body).unwrap();
        assert_eq!(result.total_results, 42);
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].repository, "tokio-rs/tokio");
        assert_eq!(result.results[0].result_id, 1);
        assert!(result.results[0].url.contains("github.com"));
        assert!(result.results[0].snippet.contains("pub fn new()"));
    }

    #[test]
    fn language_detection() {
        assert_eq!(language_from_path("src/main.rs"), "rust");
        assert_eq!(language_from_path("index.tsx"), "typescript");
        assert_eq!(language_from_path("Makefile"), "text");
    }

    #[test]
    fn truncate_respects_limits() {
        let long = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_snippet(&long);
        assert_eq!(result.lines().count(), MAX_SNIPPET_LINES);
    }

    #[test]
    fn urlencoded_basic() {
        assert_eq!(urlencoded("hello world"), "hello+world");
        assert_eq!(urlencoded("a/b"), "a%2Fb");
        assert_eq!(urlencoded("foo"), "foo");
    }
}
