//! MCP server configuration resolution: placeholder substitution and
//! transport-kind classification.

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;

use crate::context::AgentContext;
use crate::mcp_settings::McpServerConfig;
use djinn_provider::repos::CredentialRepository;

pub(super) static PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\$\{([A-Za-z0-9_]+)\}").expect("valid MCP placeholder regex"));

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedMcpServerConfig {
    pub url: Option<String>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub startup_timeout_ms: u64,
    pub request_timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum McpTransportKind {
    Http,
    Stdio,
    Unsupported,
}

#[allow(dead_code)]
impl ResolvedMcpServerConfig {
    pub fn transport_kind(&self) -> McpTransportKind {
        if self.url.is_some() {
            McpTransportKind::Http
        } else if self.command.is_some() {
            McpTransportKind::Stdio
        } else {
            McpTransportKind::Unsupported
        }
    }

    pub fn startup_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.startup_timeout_ms)
    }

    pub fn request_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.request_timeout_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MissingPlaceholder {
    pub field: String,
    pub variable: String,
}

pub(super) enum PlaceholderLookup {
    Found(String),
    Missing,
}

pub(super) async fn resolve_server_config(
    server_name: &str,
    config: &McpServerConfig,
    app_state: &AgentContext,
) -> Result<ResolvedMcpServerConfig, MissingPlaceholder> {
    Ok(ResolvedMcpServerConfig {
        url: match &config.url {
            Some(url) => Some(
                resolve_placeholder_value(app_state, url, &format!("server `{server_name}` url"))
                    .await?,
            ),
            None => None,
        },
        command: config.command.clone(),
        args: config.args.clone(),
        env: resolve_placeholder_map(
            app_state,
            &config.env,
            &format!("server `{server_name}` env"),
        )
        .await?,
        headers: resolve_placeholder_map(
            app_state,
            &config.headers,
            &format!("server `{server_name}` header"),
        )
        .await?,
        startup_timeout_ms: config.startup_timeout_ms,
        request_timeout_ms: config.request_timeout_ms,
    })
}

pub(super) async fn resolve_placeholder_map(
    app_state: &AgentContext,
    values: &HashMap<String, String>,
    field_prefix: &str,
) -> Result<HashMap<String, String>, MissingPlaceholder> {
    let mut resolved = HashMap::with_capacity(values.len());
    for (key, value) in values {
        resolved.insert(
            key.clone(),
            resolve_placeholder_value(app_state, value, &format!("{field_prefix} `{key}`")).await?,
        );
    }
    Ok(resolved)
}

pub(super) async fn resolve_placeholder_value(
    app_state: &AgentContext,
    value: &str,
    field: &str,
) -> Result<String, MissingPlaceholder> {
    let mut resolved = String::with_capacity(value.len());
    let mut last_end = 0;

    for captures in PLACEHOLDER_RE.captures_iter(value) {
        let full = captures.get(0).expect("full placeholder match");
        let variable = captures
            .get(1)
            .expect("placeholder variable capture")
            .as_str();

        resolved.push_str(&value[last_end..full.start()]);
        match lookup_placeholder_value(app_state, variable).await {
            PlaceholderLookup::Found(replacement) => resolved.push_str(&replacement),
            PlaceholderLookup::Missing => {
                return Err(MissingPlaceholder {
                    field: field.to_string(),
                    variable: variable.to_string(),
                });
            }
        }
        last_end = full.end();
    }

    if last_end == 0 {
        return Ok(value.to_string());
    }

    resolved.push_str(&value[last_end..]);
    Ok(resolved)
}

pub(super) async fn lookup_placeholder_value(
    app_state: &AgentContext,
    variable: &str,
) -> PlaceholderLookup {
    if let Ok(value) = std::env::var(variable) {
        return PlaceholderLookup::Found(value);
    }

    let credential_repo =
        CredentialRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    match credential_repo.get_decrypted(variable).await {
        Ok(Some(value)) => PlaceholderLookup::Found(value),
        Ok(None) => PlaceholderLookup::Missing,
        Err(error) => {
            tracing::warn!(
                variable = variable,
                error = %error,
                "Failed to resolve MCP placeholder from credential store"
            );
            PlaceholderLookup::Missing
        }
    }
}
