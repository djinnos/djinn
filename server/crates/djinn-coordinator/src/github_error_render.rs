//! Compatibility shim: GitHub error rendering.
//!
//! Mirrors `djinn-agent::github_error_render` for the PR poller.

use std::borrow::Cow;

use anyhow::Error;
use djinn_core::tool_error::ToolError;
use djinn_provider::github_api::{GitHubApiError, GitHubErrorSource};

const ENVELOPE_DETAIL_LIMIT: usize = 240;

pub trait GithubWriteError {
    fn github_write_envelope(&self) -> Option<Cow<'_, ToolError>>;
    fn github_write_body(&self) -> Option<&str>;
    fn github_write_status(&self) -> Option<u16>;
    fn display_string(&self) -> String;
}

impl GithubWriteError for GitHubApiError {
    fn github_write_envelope(&self) -> Option<Cow<'_, ToolError>> {
        // GitHubApiError doesn't carry a ToolError directly
        None
    }

    fn github_write_body(&self) -> Option<&str> {
        if self.body.is_empty() {
            None
        } else {
            Some(&self.body)
        }
    }

    fn github_write_status(&self) -> Option<u16> {
        self.status.map(|s| s.as_u16())
    }

    fn display_string(&self) -> String {
        format!("{self}")
    }
}

impl GithubWriteError for Error {
    fn github_write_envelope(&self) -> Option<Cow<'_, ToolError>> {
        self.downcast_ref::<GitHubApiError>()
            .and_then(|e| e.github_write_envelope())
    }

    fn github_write_body(&self) -> Option<&str> {
        self.downcast_ref::<GitHubApiError>()
            .and_then(|e| e.github_write_body())
    }

    fn github_write_status(&self) -> Option<u16> {
        self.downcast_ref::<GitHubApiError>()
            .and_then(|e| e.github_write_status())
    }

    fn display_string(&self) -> String {
        format!("{self}")
    }
}

pub fn render_github_write_error(prefix: &str, err: &(impl GithubWriteError + ?Sized)) -> String {
    match err.github_write_envelope() {
        Some(envelope) => format!("{prefix}: {}", compact_json_like_envelope(&envelope)),
        None => format!("{prefix}: {}", err.display_string()),
    }
}

fn compact_json_like_envelope(envelope: &ToolError) -> String {
    let json = serde_json::to_string(envelope).unwrap_or_default();
    if json.len() <= ENVELOPE_DETAIL_LIMIT {
        json
    } else {
        format!("{}…", &json[..ENVELOPE_DETAIL_LIMIT])
    }
}

pub fn github_write_body_contains(err: &(impl GithubWriteError + ?Sized), needle: &str) -> bool {
    err.github_write_body()
        .map(|b| b.contains(needle))
        .unwrap_or(false)
}

pub fn github_write_status_is(err: &(impl GithubWriteError + ?Sized), status: u16) -> bool {
    err.github_write_status() == Some(status)
}
