//! GitHub code search via the official GitHub Code Search API.
//!
//! Uses the user's GitHub OAuth token (from djinn sign-in) to search across
//! public GitHub repositories for code patterns, usage examples, and
//! implementations.

use djinn_provider::oauth::github_app::GitHubAppTokens;
use djinn_provider::repos::CredentialRepository;

const API_URL: &str = "https://api.github.com/search/code";
const MAX_RESULTS: usize = 15;
const MAX_SNIPPET_LINES: usize = 12;

/// Execute a code search against the GitHub Code Search API.
pub(crate) async fn search(
    cred_repo: &CredentialRepository,
    query: &str,
    language: Option<&str>,
    repo: Option<&str>,
    path: Option<&str>,
) -> Result<serde_json::Value, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("query must not be empty".into());
    }
    if query.len() > 1000 {
        return Err("query too long (max 1000 chars)".into());
    }

    // Build the GitHub code search query string.
    // Format: "query language:rust repo:owner/name path:src/"
    let mut q = query.to_string();

    if let Some(l) = language {
        let l = l.trim();
        if !l.is_empty() {
            q.push_str(&format!(" language:{l}"));
        }
    }
    if let Some(r) = repo {
        let r = r.trim();
        if !r.is_empty() {
            if !r.contains('/') || r.matches('/').count() != 1 {
                return Err("repo must be in 'owner/repo' format".into());
            }
            q.push_str(&format!(" repo:{r}"));
        }
    }
    if let Some(p) = path {
        let p = p.trim();
        if !p.is_empty() {
            q.push_str(&format!(" path:{p}"));
        }
    }

    // Load the user's GitHub OAuth token.
    let tokens = GitHubAppTokens::load_from_db(cred_repo)
        .await
        .ok_or("GitHub OAuth tokens not found — please authenticate first")?;

    let client = reqwest::Client::builder()
        .user_agent("djinn-agent/1.0")
        .build()
        .map_err(|e| format!("http client build failed: {e}"))?;

    let resp = client
        .get(API_URL)
        .query(&[("q", &q), ("per_page", &MAX_RESULTS.to_string())])
        .bearer_auth(&tokens.access_token)
        .header("Accept", "application/vnd.github.text-match+json")
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status();
    if status.as_u16() == 401 {
        return Err(
            "GitHub API returned 401 — token may have been revoked, please re-authenticate".into(),
        );
    }
    if status.as_u16() == 403 {
        // Check for rate limiting.
        let remaining = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        if remaining == Some(0) {
            return Err("GitHub API rate limit exhausted — try again shortly".into());
        }
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("GitHub API returned 403: {body}"));
    }
    if status.as_u16() == 422 {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("GitHub API rejected query (422): {body}"));
    }
    if !status.is_success() {
        return Err(format!("GitHub API returned HTTP {status}"));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("failed to parse response: {e}"))?;

    parse_response(query, &body)
}

fn parse_response(query: &str, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let total_results = body
        .get("total_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let items = body
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut results = Vec::with_capacity(MAX_RESULTS.min(items.len()));

    for item in items.iter().take(MAX_RESULTS) {
        let repository = item
            .pointer("/repository/full_name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let file_path = item
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let language = language_from_path(file_path);

        // Extract text-match fragments (requires Accept: application/vnd.github.text-match+json).
        let snippet = extract_text_matches(item);

        results.push(serde_json::json!({
            "repository": repository,
            "file_path": file_path,
            "language": language,
            "snippet": snippet,
        }));
    }

    Ok(serde_json::json!({
        "query": query,
        "total_results": total_results,
        "results_shown": results.len(),
        "results": results,
    }))
}

/// Extract and combine text-match fragments from a GitHub code search result.
fn extract_text_matches(item: &serde_json::Value) -> String {
    let text_matches = match item.get("text_matches").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return String::new(),
    };

    let mut fragments: Vec<String> = Vec::new();
    for tm in text_matches {
        if let Some(fragment) = tm.get("fragment").and_then(|v| v.as_str()) {
            let truncated = truncate_snippet(fragment);
            if !truncated.is_empty() {
                fragments.push(truncated);
            }
        }
    }
    fragments.join("\n---\n")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_detection() {
        assert_eq!(language_from_path("src/main.rs"), "rust");
        assert_eq!(language_from_path("index.tsx"), "typescript");
        assert_eq!(language_from_path("Makefile"), "text");
    }

    #[test]
    fn truncate_respects_limits() {
        let long = (0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let result = truncate_snippet(&long);
        assert_eq!(result.lines().count(), MAX_SNIPPET_LINES);
    }

    #[test]
    fn parse_empty_response() {
        let body = serde_json::json!({});
        let result = parse_response("test", &body).unwrap();
        assert_eq!(result["total_results"], 0);
        assert_eq!(result["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn parse_typical_response() {
        let body = serde_json::json!({
            "total_count": 42,
            "items": [{
                "repository": { "full_name": "tokio-rs/tokio" },
                "path": "tokio/src/runtime/mod.rs",
                "text_matches": [{
                    "fragment": "pub fn new() -> Runtime {\n    // create runtime\n}"
                }]
            }]
        });
        let result = parse_response("pub fn new", &body).unwrap();
        assert_eq!(result["total_results"], 42);
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["results"][0]["repository"], "tokio-rs/tokio");
        assert_eq!(result["results"][0]["language"], "rust");
        assert!(
            result["results"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("pub fn new()")
        );
    }

    #[test]
    fn extract_text_matches_empty() {
        let item = serde_json::json!({});
        assert_eq!(extract_text_matches(&item), "");
    }

    #[test]
    fn extract_text_matches_multiple() {
        let item = serde_json::json!({
            "text_matches": [
                { "fragment": "first match" },
                { "fragment": "second match" }
            ]
        });
        let result = extract_text_matches(&item);
        assert!(result.contains("first match"));
        assert!(result.contains("second match"));
        assert!(result.contains("---"));
    }
}
