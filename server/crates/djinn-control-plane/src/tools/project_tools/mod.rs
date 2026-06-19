// MCP tools for project lifecycle, configuration, indexer, and GitHub integration.
//
// This module is composed of four focused submodules, each owning one
// operation family. The composite `project_tool_router` re-exposes the
// individual sub-routers through `DjinnMcpServer` so the rest of the
// server sees a single `Self::project_tool_router()` entry point.
//
//   * `lifecycle_ops` — project add / remove / list / branches
//   * `config_ops`    — project config, environment config, verification
//   * `indexer_ops`   — graph exclusions, stack, devcontainer status,
//                       image rebuild, workspace warm status
//   * `github_ops`    — list repos from the bound GitHub App installation
//
// The submodule-level `pub` items are re-exported here unchanged so
// downstream consumers (e.g. `crate::dispatch`) can keep using
// `crate::tools::project_tools::<Name>` paths.

mod config_ops;
mod github_ops;
mod indexer_ops;
mod lifecycle_ops;

pub use self::config_ops::{
    ProjectConfigGetParams, ProjectConfigResponse, ProjectConfigSetParams,
    ProjectEnvironmentConfigGetParams, ProjectEnvironmentConfigGetResponse,
    ProjectEnvironmentConfigResetParams, ProjectEnvironmentConfigResetResponse,
    ProjectEnvironmentConfigSetParams, ProjectEnvironmentConfigSetResponse,
};
pub use self::github_ops::{GithubListReposParams, GithubListReposResponse, GithubRepoEntry};
pub use self::indexer_ops::{
    GetProjectDevcontainerStatusParams, GetProjectDevcontainerStatusResponse,
    GetProjectStackParams, GetProjectStackResponse, ProjectGraphExclusionsGetParams,
    ProjectGraphExclusionsResponse, ProjectGraphExclusionsSetParams, RetriggerImageBuildParams,
    RetriggerImageBuildResponse, WorkspaceWarmStatusEntry,
};
pub use self::lifecycle_ops::{
    ProjectAddFromGithubParams, ProjectAddResponse, ProjectBranchesParams, ProjectBranchesResponse,
    ProjectInfo, ProjectListResponse, ProjectRemoveParams, ProjectRemoveResponse,
};

// ── Router composition ────────────────────────────────────────────────────────

impl crate::server::DjinnMcpServer {
    /// Composite router for all project tools (lifecycle + config + indexer + github).
    pub fn project_tool_router() -> rmcp::handler::server::router::tool::ToolRouter<Self> {
        Self::lifecycle_tool_router()
            + Self::config_tool_router()
            + Self::indexer_tool_router()
            + Self::project_github_tool_router()
    }
}
