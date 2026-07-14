//! MCP startup connection and diagnostic boundaries.
//!
//! This module keeps raw transport errors for legacy structured logging while
//! producing only bounded, canonical facts for extension-load diagnostics.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use reqwest::header::{HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::service::{Peer, RoleClient};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};

use crate::extension_diagnostics::ExtensionDiagnosticFact;
use djinn_core::extension_diagnostics::{
    ExtensionLoadPhase, ExtensionLoadRemedyCode, ExtensionLoadSeverity, ExtensionLoadSourceKind,
};

use super::{McpNotificationHandler, RoutingState};

/// The reachable startup boundaries exposed by this HTTP-only loader.
#[derive(Debug, Clone)]
pub(super) enum McpStartupFailure {
    Transport { error: String },
    // rmcp serves connection and initialize at the same boundary.
    Handshake { error: String },
    ToolsList { error: String },
}

impl std::fmt::Display for McpStartupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Preserve the underlying startup error in the existing structured
            // log field. The fact intentionally has only a bounded, trusted
            // summary because transport errors can contain remote data.
            Self::Transport { error } | Self::Handshake { error } | Self::ToolsList { error } => {
                formatter.write_str(error)
            }
        }
    }
}

impl McpStartupFailure {
    pub(super) fn diagnostic(&self, server_name: &str) -> ExtensionDiagnosticFact {
        let (phase, remedy_code, summary_material) = match self {
            Self::Transport { .. } => (
                ExtensionLoadPhase::Transport,
                ExtensionLoadRemedyCode::CheckTransport,
                "MCP transport configuration could not be initialized.",
            ),
            Self::Handshake { .. } => (
                ExtensionLoadPhase::Handshake,
                ExtensionLoadRemedyCode::CheckServer,
                "MCP connection or initialization failed.",
            ),
            Self::ToolsList { .. } => (
                ExtensionLoadPhase::ToolsList,
                ExtensionLoadRemedyCode::CheckServer,
                "Initial MCP tools/list request failed.",
            ),
        };
        mcp_diagnostic(server_name, phase, remedy_code, summary_material)
    }
}

pub(super) fn mcp_diagnostic(
    server_name: &str,
    phase: ExtensionLoadPhase,
    remedy_code: ExtensionLoadRemedyCode,
    summary_material: &'static str,
) -> ExtensionDiagnosticFact {
    ExtensionDiagnosticFact {
        source_kind: ExtensionLoadSourceKind::ProjectMcp,
        source_key: server_name.to_owned(),
        phase,
        severity: ExtensionLoadSeverity::Warning,
        remedy_code,
        // Facts never include raw URL/header/command/args/env/stderr or rmcp errors.
        summary_material: summary_material.to_owned(),
    }
}

/// Establish a connection to an MCP server via Streamable HTTP transport.
///
/// The returned peer uses a [`McpNotificationHandler`] that observes
/// `LoggingMessageNotification`s from the server and emits them through host
/// tracing with structured server, logger, level, and task identifiers. The
/// handler also processes `tools/list_changed` notifications using `routing`.
pub(super) async fn connect_to_server(
    url: &str,
    headers: &HashMap<String, String>,
    server_name: &str,
    task_short_id: &str,
    routing: Arc<RwLock<RoutingState>>,
) -> Result<Peer<RoleClient>, McpStartupFailure> {
    let mut custom_headers = HashMap::new();
    for (name, value) in headers {
        let header_name = HeaderName::try_from(name.as_str()).map_err(|error| {
            McpStartupFailure::Transport {
                error: error.to_string(),
            }
        })?;
        let header_value = HeaderValue::try_from(value.as_str()).map_err(|error| {
            McpStartupFailure::Transport {
                error: error.to_string(),
            }
        })?;
        custom_headers.insert(header_name, header_value);
    }

    let config = StreamableHttpClientTransportConfig::with_uri(url.to_string())
        .custom_headers(custom_headers);
    let transport = StreamableHttpClientTransport::from_config(config);

    let handler = McpNotificationHandler {
        server_name: server_name.to_string(),
        task_short_id: task_short_id.to_string(),
        routing,
    };
    let service = handler
        .serve(transport)
        .await
        .map_err(|error| McpStartupFailure::Handshake {
            error: error.to_string(),
        })?;
    let peer = service.peer().clone();
    // Keep the service alive in the background so notification processing
    // continues for the lifetime of the connection.
    tokio::spawn(async move {
        let _ = service.waiting().await;
    });
    Ok(peer)
}

/// Combined connect + initialize + initial `tools/list` in a single future.
///
/// This is the unit that `connect_and_discover` wraps with
/// `startup_timeout_ms`, so timeout cancellation covers both transport startup
/// and initial tool enumeration.
pub(super) async fn startup_and_list(
    url: &str,
    headers: &HashMap<String, String>,
    server_name: &str,
    task_short_id: &str,
    routing: Arc<RwLock<RoutingState>>,
) -> Result<(Peer<RoleClient>, rmcp::model::ListToolsResult), McpStartupFailure> {
    let peer = connect_to_server(url, headers, server_name, task_short_id, routing).await?;
    let result = peer
        .list_tools(None)
        .await
        .map_err(|error| McpStartupFailure::ToolsList {
            error: error.to_string(),
        })?;
    Ok((peer, result))
}
