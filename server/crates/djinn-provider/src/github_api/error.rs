use std::fmt;

use reqwest::StatusCode;

/// Source classification for a GitHub API error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHubErrorSource {
    /// Typed HTTP status with a body (e.g. 4xx/5xx JSON error).
    Http,
    /// 429 (rate-limited) or X-RateLimit-Remaining=0 — caller should back off.
    RateLimited,
    /// Transport-layer failure (connection reset, DNS, timeout).
    Transport,
    /// 401 / token revoked — caller should fall back to re-auth path.
    Unauthenticated,
    /// GraphQL errors array (e.g. UNPROCESSABLE for enqueuePullRequest).
    GraphQL,
}

/// Typed GitHub API error. Carries the structured shape that callers can
/// classify without re-parsing a stringified anyhow display chain.
#[derive(Debug)]
pub struct GitHubApiError {
    /// Logical operation that failed, e.g. "create_pull_request".
    pub method: &'static str,
    /// Targeted resource, e.g. "owner/repo" or "owner/repo:pulls/123".
    pub path: String,
    /// Typed HTTP status. `None` for transport / GraphQL errors.
    pub status: Option<StatusCode>,
    /// Raw response body, or GraphQL `errors` array serialised to JSON.
    pub body: String,
    /// Source classification (Http, RateLimited, Transport, Unauthenticated, GraphQL).
    pub source: GitHubErrorSource,
}

impl GitHubApiError {
    pub fn http(method: &'static str, path: String, status: StatusCode, body: String) -> Self {
        let source = if status == StatusCode::UNAUTHORIZED {
            GitHubErrorSource::Unauthenticated
        } else if status == StatusCode::TOO_MANY_REQUESTS {
            GitHubErrorSource::RateLimited
        } else {
            GitHubErrorSource::Http
        };
        Self {
            method,
            path,
            status: Some(status),
            body,
            source,
        }
    }

    pub fn rate_limited(method: &'static str, path: String, body: String) -> Self {
        Self {
            method,
            path,
            status: Some(StatusCode::TOO_MANY_REQUESTS),
            body,
            source: GitHubErrorSource::RateLimited,
        }
    }

    pub fn transport(method: &'static str, path: String, body: String) -> Self {
        Self {
            method,
            path,
            status: None,
            body,
            source: GitHubErrorSource::Transport,
        }
    }

    pub fn unauthenticated(method: &'static str, path: String, body: String) -> Self {
        Self {
            method,
            path,
            status: Some(StatusCode::UNAUTHORIZED),
            body,
            source: GitHubErrorSource::Unauthenticated,
        }
    }

    pub fn graphql(method: &'static str, path: String, body: String) -> Self {
        Self {
            method,
            path,
            status: None,
            body,
            source: GitHubErrorSource::GraphQL,
        }
    }

    /// True iff the body contains the 422-already-exists recovery marker.
    pub fn is_pr_already_exists(&self) -> bool {
        self.status.map(|s| s.as_u16() == 422).unwrap_or(false)
            && self.body.contains("already exists")
    }
}

impl fmt::Display for GitHubApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.source == GitHubErrorSource::GraphQL {
            return write!(f, "{} GraphQL error: {}", self.method, self.body);
        }
        let status = self
            .status
            .map(|s| s.as_u16().to_string())
            .unwrap_or_else(|| "<no-status>".into());
        write!(
            f,
            "github {} {} failed: {}{}",
            self.method,
            self.path,
            status,
            excerpt_for_display(&self.body)
        )
    }
}

impl std::error::Error for GitHubApiError {}

fn excerpt_for_display(body: &str) -> String {
    const MAX: usize = 200;
    if body.len() <= MAX {
        body.to_string()
    } else {
        format!("{}…[+{} chars]", &body[..MAX], body.len() - MAX)
    }
}
