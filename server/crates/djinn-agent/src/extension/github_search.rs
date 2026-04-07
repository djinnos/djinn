//! GitHub code search via grep.app — built-in agent tool.
//!
//! Wraps the public grep.app API to let agents search across millions of
//! GitHub repositories for code patterns, usage examples, and implementations.

use regex::Regex;
use std::sync::LazyLock;

const API_URL: &str = "https://grep.app/api/search";
const MAX_RESULTS: usize = 15;
const MAX_SNIPPET_CHARS: usize = 600;
const MAX_SNIPPET_LINES: usize = 12;

static HTML_TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static DATA_LINE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-line="(\d+)""#).unwrap());

/// Execute a search against grep.app and return structured JSON.
pub(crate) async fn search(
    client: &reqwest::Client,
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

    // Build URL with query parameters.
    let mut url = format!("{}?q={}", API_URL, urlencoded(query));
    if let Some(l) = language {
        let l = l.trim();
        if !l.is_empty() {
            url.push_str(&format!("&f.lang={}", urlencoded(l)));
        }
    }
    if let Some(r) = repo {
        let r = r.trim();
        if !r.is_empty() {
            if !r.contains('/') || r.matches('/').count() != 1 {
                return Err("repo must be in 'owner/repo' format".into());
            }
            url.push_str(&format!("&f.repo={}", urlencoded(r)));
        }
    }
    if let Some(p) = path {
        let p = p.trim();
        if !p.is_empty() {
            url.push_str(&format!("&f.path={}", urlencoded(p)));
        }
    }

    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = resp.status();
    if status.as_u16() == 429 {
        return Err("rate limited by grep.app — try again shortly".into());
    }
    if !status.is_success() {
        return Err(format!("grep.app returned HTTP {status}"));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("failed to parse response: {e}"))?;

    parse_response(query, &body)
}

fn parse_response(query: &str, body: &serde_json::Value) -> Result<serde_json::Value, String> {
    let total_results = body
        .pointer("/facets/count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let hits = body
        .pointer("/hits/hits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut results = Vec::with_capacity(MAX_RESULTS.min(hits.len()));

    for hit in hits.iter().take(MAX_RESULTS) {
        let repository = hit
            .pointer("/repo/raw")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let file_path = hit
            .pointer("/path/raw")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let branch = hit
            .pointer("/branch/raw")
            .and_then(|v| v.as_str())
            .unwrap_or("main");

        let html_snippet = hit
            .pointer("/content/snippet")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let line_numbers = extract_line_numbers(html_snippet);
        let clean_text = clean_html(html_snippet);
        let language = language_from_path(file_path);
        let snippet = truncate_snippet(&clean_text);

        results.push(serde_json::json!({
            "repository": repository,
            "file_path": file_path,
            "branch": branch,
            "line_numbers": line_numbers,
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

fn clean_html(html: &str) -> String {
    let text = HTML_TAG_RE.replace_all(html, "");
    text.replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn extract_line_numbers(html: &str) -> Vec<u32> {
    DATA_LINE_RE
        .captures_iter(html)
        .filter_map(|cap| cap[1].parse().ok())
        .collect()
}

fn truncate_snippet(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut char_count = 0;

    for line in text.lines() {
        let trimmed = line.trim_end();
        if char_count + trimmed.len() > MAX_SNIPPET_CHARS {
            break;
        }
        lines.push(trimmed);
        char_count += trimmed.len() + 1;
        if lines.len() >= MAX_SNIPPET_LINES {
            break;
        }
    }

    lines.join("\n")
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
    fn clean_html_strips_tags() {
        let html = r#"<span class="hl">fn</span> main() {}"#;
        assert_eq!(clean_html(html), "fn main() {}");
    }

    #[test]
    fn clean_html_decodes_entities() {
        assert_eq!(clean_html("a &amp; b &lt; c"), "a & b < c");
    }

    #[test]
    fn extract_line_numbers_works() {
        let html = r#"<tr data-line="10"><td>code</td></tr><tr data-line="25"><td>more</td></tr>"#;
        assert_eq!(extract_line_numbers(html), vec![10, 25]);
    }

    #[test]
    fn language_detection() {
        assert_eq!(language_from_path("src/main.rs"), "rust");
        assert_eq!(language_from_path("index.tsx"), "typescript");
        assert_eq!(language_from_path("Makefile"), "text");
    }

    #[test]
    fn truncate_respects_limits() {
        let long = "a".repeat(700);
        let result = truncate_snippet(&long);
        assert!(result.len() <= MAX_SNIPPET_CHARS);
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
            "facets": { "count": 42 },
            "hits": {
                "hits": [{
                    "repo": { "raw": "tokio-rs/tokio" },
                    "path": { "raw": "tokio/src/runtime/mod.rs" },
                    "branch": { "raw": "main" },
                    "content": {
                        "snippet": "<tr data-line=\"10\"><td><span class=\"hl\">pub</span> fn new()</td></tr>"
                    }
                }]
            }
        });
        let result = parse_response("pub fn new", &body).unwrap();
        assert_eq!(result["total_results"], 42);
        assert_eq!(result["results"].as_array().unwrap().len(), 1);
        assert_eq!(result["results"][0]["repository"], "tokio-rs/tokio");
        assert_eq!(result["results"][0]["language"], "rust");
        assert_eq!(
            result["results"][0]["line_numbers"],
            serde_json::json!([10])
        );
        assert!(
            result["results"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("pub fn new()")
        );
    }

    #[test]
    fn urlencoded_basic() {
        assert_eq!(urlencoded("hello world"), "hello+world");
        assert_eq!(urlencoded("a/b"), "a%2Fb");
        assert_eq!(urlencoded("foo"), "foo");
    }
}
