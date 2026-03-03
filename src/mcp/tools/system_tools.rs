use std::fs;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::logging;
use crate::mcp::server::DjinnMcpServer;

#[derive(Serialize, JsonSchema)]
pub struct PingResponse {
    pub status: &'static str,
    pub version: &'static str,
}

#[derive(Deserialize, JsonSchema)]
pub struct SystemLogsInput {
    pub lines: Option<usize>,
    pub level: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct SystemLogsResponse {
    pub file: String,
    pub lines: Vec<String>,
    pub filtered: bool,
    pub error: Option<String>,
}

#[tool_router(router = system_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Ping the server. Returns {status: ok, version}.
    #[tool(description = "Ping the server to check if it's alive")]
    pub async fn system_ping(&self) -> Json<PingResponse> {
        Json(PingResponse {
            status: "ok",
            version: env!("CARGO_PKG_VERSION"),
        })
    }

    /// Read recent operational logs from disk (useful for remote servers).
    #[tool(
        description = "Read recent operational log lines from the rotating file logger. Supports optional line count and level filtering."
    )]
    pub async fn system_logs(
        &self,
        Parameters(input): Parameters<SystemLogsInput>,
    ) -> Json<SystemLogsResponse> {
        let max_lines = input.lines.unwrap_or(200).clamp(1, 2_000);
        let level_filter = input.level.as_ref().map(|level| level.to_uppercase());

        let Some(path) = logging::latest_log_file_path() else {
            return Json(SystemLogsResponse {
                file: String::new(),
                lines: vec![],
                filtered: level_filter.is_some(),
                error: Some("no log file found".to_string()),
            });
        };

        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                return Json(SystemLogsResponse {
                    file: path.display().to_string(),
                    lines: vec![],
                    filtered: level_filter.is_some(),
                    error: Some(e.to_string()),
                });
            }
        };

        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

        if let Some(level) = level_filter.as_deref() {
            let needle = format!(" {level} ");
            lines.retain(|line| line.contains(&needle));
        }

        let start = lines.len().saturating_sub(max_lines);

        Json(SystemLogsResponse {
            file: path.display().to_string(),
            lines: lines[start..].to_vec(),
            filtered: level_filter.is_some(),
            error: None,
        })
    }
}
