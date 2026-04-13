//! MCP tools for GitHub code search and file fetching.

use std::sync::Arc;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use djinn_provider::github_api::GitHubApiClient;
use djinn_provider::github_api::search::{CodeSearchHit, CodeSearchResult, FileFetchResult};
use djinn_provider::repos::CredentialRepository;

use crate::server::DjinnMcpServer;
use crate::tools::task_tools::{ErrorOr, ErrorResponse};

fn parse_optional_positive_usize(value: Option<i64>, field: &str) -> Result<Option<usize>, String> {
    match value {
        None => Ok(None),
        Some(value) if value <= 0 => Err(format!("{field} must be greater than 0")),
        Some(value) => usize::try_from(value)
            .map(Some)
            .map_err(|_| format!("{field} is too large")),
    }
}

fn parse_optional_positive_u32(value: Option<i64>, field: &str) -> Result<Option<u32>, String> {
    match value {
        None => Ok(None),
        Some(value) if value <= 0 => Err(format!("{field} must be greater than 0")),
        Some(value) => u32::try_from(value)
            .map(Some)
            .map_err(|_| format!("{field} is too large")),
    }
}

// ── Request types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GithubSearchParams {
    /// Search query string. Supports GitHub code search syntax.
    pub query: String,
    /// Programming language filter (e.g. "Rust", "Python", "TypeScript").
    #[serde(default)]
    pub language: Option<String>,
    /// Repository filter in "owner/repo" format (e.g. "tokio-rs/tokio").
    #[serde(default)]
    pub repo: Option<String>,
    /// Path filter to search within specific directories (e.g. "src/").
    #[serde(default)]
    pub path: Option<String>,
    /// Maximum number of results to return (1–100, default 15).
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GithubFetchFileParams {
    /// Repository in "owner/repo" format (e.g. "tokio-rs/tokio").
    pub repo: String,
    /// File path within the repository (e.g. "src/lib.rs").
    pub path: String,
    /// Branch, tag, or commit SHA (default: HEAD / default branch).
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    /// First line to return (1-based, inclusive). Omit for start of file.
    #[serde(default)]
    pub start_line: Option<i64>,
    /// Last line to return (1-based, inclusive). Omit for end of file.
    #[serde(default)]
    pub end_line: Option<i64>,
<<<<<<< HEAD
}

fn normalize_positive_usize(value: Option<i64>, field: &str) -> Result<Option<usize>, String> {
    match value {
        Some(value) if value < 1 => Err(format!("{field} must be at least 1")),
        Some(value) => usize::try_from(value)
            .map(Some)
            .map_err(|_| format!("{field} is too large")),
        None => Ok(None),
    }
}

fn normalize_positive_u32(value: Option<i64>, field: &str) -> Result<Option<u32>, String> {
    match value {
        Some(value) if value < 1 => Err(format!("{field} must be at least 1")),
        Some(value) => u32::try_from(value)
            .map(Some)
            .map_err(|_| format!("{field} is too large")),
        None => Ok(None),
    }
=======
>>>>>>> origin/main
}

// ── Response types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GithubSearchResponse {
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
    pub results: Vec<SearchHitResponse>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchHitResponse {
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

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct GithubFetchFileResponse {
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

// ── Conversions ───────────────────────────────────────────────────────────────

impl From<CodeSearchResult> for GithubSearchResponse {
    fn from(r: CodeSearchResult) -> Self {
        Self {
            query: r.query,
            language: r.language,
            repo: r.repo,
            path_filter: r.path_filter,
            total_results: r.total_results,
            results_shown: r.results_shown,
            truncated: r.truncated,
            results: r.results.into_iter().map(SearchHitResponse::from).collect(),
        }
    }
}

impl From<CodeSearchHit> for SearchHitResponse {
    fn from(h: CodeSearchHit) -> Self {
        Self {
            result_id: h.result_id,
            repository: h.repository,
            path: h.path,
            language: h.language,
            line_number: h.line_number,
            snippet: h.snippet,
            url: h.url,
            git_ref: h.git_ref,
            score: h.score,
        }
    }
}

impl From<FileFetchResult> for GithubFetchFileResponse {
    fn from(r: FileFetchResult) -> Self {
        Self {
            repository: r.repository,
            path: r.path,
            git_ref: r.git_ref,
            url: r.url,
            size_bytes: r.size_bytes,
            start_line: r.start_line,
            end_line: r.end_line,
            truncated: r.truncated,
            content: r.content,
        }
    }
}

// ── Handler ───────────────────────────────────────────────────────────────────

#[tool_router(router = github_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Search GitHub code across public repositories. Returns compact,
    /// navigable matches with snippets, URLs, and metadata. Use
    /// `github_fetch_file` to open a full file from the results.
    #[tool(
        description = "Search GitHub code across public repositories. Returns compact, navigable matches with snippets, file paths, URLs, and metadata suitable for browsing. Each result has a result_id for reference. Use github_fetch_file to inspect the full file of a promising result. Supports language, repo, and path filters."
    )]
    pub async fn github_search(
        &self,
        Parameters(params): Parameters<GithubSearchParams>,
    ) -> Json<ErrorOr<GithubSearchResponse>> {
        let limit = match normalize_positive_usize(params.limit, "limit") {
            Ok(limit) => limit,
            Err(error) => return Json(ErrorOr::Error(ErrorResponse { error })),
        };

        let cred_repo = Arc::new(CredentialRepository::new(
            self.state.db().clone(),
            self.state.event_bus(),
        ));
        let client = GitHubApiClient::new(cred_repo);

        let limit = match parse_optional_positive_usize(params.limit, "limit") {
            Ok(limit) => limit,
            Err(error) => return Json(ErrorOr::Error(ErrorResponse { error })),
        };

        match client
            .search_code(
                &params.query,
                params.language.as_deref(),
                params.repo.as_deref(),
                params.path.as_deref(),
                limit,
            )
            .await
        {
            Ok(result) => Json(ErrorOr::Ok(GithubSearchResponse::from(result))),
            Err(e) => Json(ErrorOr::Error(ErrorResponse {
                error: e.to_string(),
            })),
        }
    }

    /// Fetch the contents of a file from a GitHub repository. Supports
    /// optional line-range selection for large files.
    #[tool(
        description = "Fetch the full contents of a file from a public GitHub repository. Use after github_search to inspect a promising result. Supports optional start_line/end_line for reading specific sections of large files. Returns the file content with metadata including size, ref, and URL."
    )]
    pub async fn github_fetch_file(
        &self,
        Parameters(params): Parameters<GithubFetchFileParams>,
    ) -> Json<ErrorOr<GithubFetchFileResponse>> {
        let start_line = match normalize_positive_u32(params.start_line, "start_line") {
            Ok(start_line) => start_line,
            Err(error) => return Json(ErrorOr::Error(ErrorResponse { error })),
        };
        let end_line = match normalize_positive_u32(params.end_line, "end_line") {
            Ok(end_line) => end_line,
            Err(error) => return Json(ErrorOr::Error(ErrorResponse { error })),
        };

        let cred_repo = Arc::new(CredentialRepository::new(
            self.state.db().clone(),
            self.state.event_bus(),
        ));
        let client = GitHubApiClient::new(cred_repo);

        let start_line = match parse_optional_positive_u32(params.start_line, "start_line") {
            Ok(start_line) => start_line,
            Err(error) => return Json(ErrorOr::Error(ErrorResponse { error })),
        };
        let end_line = match parse_optional_positive_u32(params.end_line, "end_line") {
            Ok(end_line) => end_line,
            Err(error) => return Json(ErrorOr::Error(ErrorResponse { error })),
        };

        match client
            .fetch_file(
                &params.repo,
                &params.path,
                params.git_ref.as_deref(),
                start_line,
                end_line,
            )
            .await
        {
            Ok(result) => Json(ErrorOr::Ok(GithubFetchFileResponse::from(result))),
            Err(e) => Json(ErrorOr::Error(ErrorResponse {
                error: e.to_string(),
            })),
        }
    }
}
