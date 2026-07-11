use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::lifecycle_ops::resolve_project;
use crate::server::DjinnMcpServer;
use djinn_db::{
    CODELESS_WORKSPACE_SLUG, Database, ProjectImageStatus, ProjectRepository,
    ProjectWorkspaceGraphRepository, RepoGraphCacheRepository,
};
use djinn_stack::Stack;

// ── Helpers ──────────────────────────────────────────────────────────────────

async fn graph_warmed_at_from_freshness(db: Database, project_id: &str) -> Option<String> {
    if let Some(workspace) = ProjectWorkspaceGraphRepository::new(db.clone())
        .latest_warmed_state(project_id)
        .await
        .ok()
        .flatten()
        .filter(|workspace| !workspace.warmed_at.is_empty())
    {
        return Some(workspace.warmed_at);
    }

    RepoGraphCacheRepository::new(db)
        .latest_for_project(project_id)
        .await
        .ok()
        .flatten()
        .map(|row| row.built_at)
}

/// Derive the banner's `graph_warm_status` from image status + warm stamp.
fn derive_graph_warm_status(image_status: &str, graph_warmed_at: &Option<String>) -> String {
    if graph_warmed_at.is_some() {
        return "ready".to_string();
    }
    match image_status {
        s if s == ProjectImageStatus::FAILED => "failed".to_string(),
        s if s == ProjectImageStatus::READY => "running".to_string(),
        _ => "pending".to_string(),
    }
}

fn derive_graph_warm_status_with_workspaces(
    image_status: &str,
    graph_warmed_at: &Option<String>,
    workspace_statuses: &[WorkspaceWarmStatusEntry],
) -> String {
    // A `failed`/`timed_out` workspace row means the indexer produced no
    // SCIP artifact for that workspace. The warm's partial-success policy
    // (`tally_indexer_results`) intentionally keeps the *overall* warm
    // alive when some other workspace succeeds, so this is not necessarily
    // a total failure — but the degradation MUST be visible instead of
    // being masked as `ready`.
    //
    //   * If the project graph still warmed from other workspaces
    //     (`graph_warmed_at` is set), surface `degraded`: the graph is
    //     usable but incomplete, and the failing workspace(s) are listed
    //     in `workspace_warm_statuses`.
    //   * If nothing warmed at all, it is a hard `failed`.
    //
    // Failed/timed_out is checked first so it always wins over a shrink
    // `warning`, mirroring the suppression in
    // `canonical_graph::detect_graph_cache_shrink_warning`.
    if workspace_statuses
        .iter()
        .any(|entry| matches!(entry.status.as_str(), "failed" | "timed_out"))
    {
        return if graph_warmed_at.is_some() {
            "degraded".to_string()
        } else {
            "failed".to_string()
        };
    }
    // A non-fatal graph-cache shrink warning surfaces as a workspace row
    // carrying status `warning`. It is warning-level (not failure-level) —
    // preserve `graph_warmed_at` freshness, but surface it as `warning` so
    // the UI can prompt operators to inspect the matching workspace row.
    if workspace_statuses
        .iter()
        .any(|entry| entry.status.as_str() == "warning")
    {
        return "warning".to_string();
    }
    derive_graph_warm_status(image_status, graph_warmed_at)
}

/// Build the per-workspace warm-status list from the durable
/// `project_workspace_graph` rows — the SAME source the `code_graph
/// workspaces` op reads. The previous implementation read a
/// `.djinn/graph_warm_status.json` file written by the warm pod, which the
/// control plane never sees in pod mode, so the list was always empty and a
/// `timed_out` workspace stayed silently masked as `ready`.
async fn workspace_warm_statuses_from_db(
    db: Database,
    project_id: &str,
) -> Vec<WorkspaceWarmStatusEntry> {
    match ProjectWorkspaceGraphRepository::new(db)
        .list_for_project(project_id)
        .await
    {
        Ok(rows) => rows
            .into_iter()
            // The code-less sentinel is a freshness marker ("nothing to
            // warm = considered warmed"), not a real workspace — same
            // filter the `workspaces` op applies.
            .filter(|row| row.workspace_slug != CODELESS_WORKSPACE_SLUG)
            .map(|row| WorkspaceWarmStatusEntry {
                workspace_slug: row.workspace_slug,
                status: row.status,
                commit_sha: Some(row.commit_sha).filter(|sha| !sha.is_empty()),
                warmed_at: Some(row.warmed_at).filter(|at| !at.is_empty()),
            })
            .collect(),
        Err(err) => {
            tracing::warn!(project = %project_id, error = %err, "failed to read project_workspace_graph rows for warm statuses");
            Vec::new()
        }
    }
}

/// Build a `GetProjectDevcontainerStatusResponse` for the error/early-exit
/// paths. Keeps the many early-return sites short + consistent.
fn error_response(
    image_status: String,
    image_last_error: Option<String>,
    error: String,
) -> GetProjectDevcontainerStatusResponse {
    let graph_warm_status = derive_graph_warm_status(&image_status, &None);
    GetProjectDevcontainerStatusResponse {
        image_tag: None,
        image_status,
        needs_image: false,
        image_last_error,
        graph_warmed_at: None,
        graph_warm_status,
        workspace_warm_statuses: Vec::new(),
        error: Some(error),
    }
}

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProjectGraphExclusionsGetParams {
    /// Project path (same shape accepted by `project_config_get`).
    pub project: String,
}

/// Partial-update payload. Either field may be `None` — the repository
/// leaves the corresponding column untouched when the caller only wants
/// to rewrite one of the two lists.
#[derive(Deserialize, JsonSchema)]
#[schemars(
    description = "Partial-update payload. Either field may be `None` — the repository\nleaves the corresponding column untouched when the caller only wants\nto rewrite one of the two lists."
)]
pub struct ProjectGraphExclusionsSetParams {
    pub project: String,
    /// When present, replaces `graph_excluded_paths` wholesale. Empty
    /// vec resets to zero globs (Tier 1 still applies).
    #[serde(default)]
    pub graph_excluded_paths: Option<Vec<String>>,
    /// When present, replaces `graph_orphan_ignore` wholesale.
    #[serde(default)]
    pub graph_orphan_ignore: Option<Vec<String>>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetProjectStackParams {
    /// Project UUID whose detected stack should be returned.
    pub project: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct GetProjectDevcontainerStatusParams {
    /// Project UUID whose devcontainer + image status should be returned.
    pub project: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct RetriggerImageBuildParams {
    /// Project UUID whose image should be rebuilt on the next mirror-fetch tick.
    pub project: String,
}

// ── Response structs ─────────────────────────────────────────────────────────

#[derive(Serialize, JsonSchema)]
pub struct ProjectGraphExclusionsResponse {
    pub status: String,
    pub project: String,
    /// Glob patterns stripped from cycles/orphans/ranked/impact/edges
    /// output. Empty array means no per-project globs; Tier 1 (universal
    /// SCIP-artifact filter) still applies.
    #[serde(default)]
    pub graph_excluded_paths: Vec<String>,
    /// Exact file paths the `code_graph orphans` op silently drops.
    #[serde(default)]
    pub graph_orphan_ignore: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct GetProjectStackResponse {
    /// Detected stack metadata, or `None` when the project exists but no
    /// detection has run yet (default `{}` in the DB) or when the
    /// persisted JSON fails to deserialize.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<Stack>,
    /// Populated on lookup / deserialization failures; clients should
    /// surface this verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Snapshot of a project's image-build state, used by the UI onboarding
/// banner.
#[derive(Serialize, JsonSchema)]
pub struct GetProjectDevcontainerStatusResponse {
    /// Content-addressable image tag from the last successful build, or
    /// `None` when no build has completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_tag: Option<String>,
    /// One of `none | building | ready | failed`. Reflects the project's
    /// assigned **catalog** image (migration 46) — projects no longer build a
    /// bespoke per-project image.
    pub image_status: String,
    /// `true` when the project has no catalog image assigned yet — it needs
    /// setup (pick an image) before it can build/dispatch. The UI surfaces
    /// this as a distinct "Needs image" state rather than a build status.
    pub needs_image: bool,
    /// Human-readable error from the most recent failed build, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_last_error: Option<String>,
    /// ISO-8601 UTC timestamp derived from per-workspace graph freshness
    /// or the merged repo graph cache. `None` means no freshness source has
    /// completed yet (cold project or failing pipeline). Dispatch no longer
    /// blocks on this value; it is badge metadata retained under a
    /// compatibility field name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_warmed_at: Option<String>,
    /// Derived status for the UI banner. One of
    /// `pending | running | ready | warning | degraded | failed`.
    /// `pending` means no warm has ever run; `running` means the image is
    /// ready and a warm should be in flight (or imminent); `ready` means
    /// derived graph freshness exists and every workspace warmed cleanly;
    /// `warning` means a non-fatal graph-cache shrink backstop fired — the
    /// cache is still fresh and graph usage is unaffected, but operators
    /// should inspect the matching `workspace_warm_statuses` row;
    /// `degraded` means the overall graph warmed but at least one workspace
    /// row is `failed`/`timed_out` (partial success — the graph is usable
    /// but incomplete; the failing workspaces are listed in
    /// `workspace_warm_statuses`); `failed` means the image build failed
    /// (no warm possible) or a `failed`/`timed_out` workspace exists with
    /// no successful warm at all. `failed`/`timed_out` always override a
    /// shrink warning.
    pub graph_warm_status: String,
    /// Per-workspace graph warm results, sourced from the durable
    /// `project_workspace_graph` rows (the same source the `code_graph
    /// workspaces` op reads). Entries with `failed` or `timed_out` explain
    /// which workspace did not produce a SCIP artifact even when another
    /// workspace succeeded and the overall warm reports `degraded`.
    #[serde(default)]
    pub workspace_warm_statuses: Vec<WorkspaceWarmStatusEntry>,
    /// Populated on lookup failures; clients should surface this verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A single per-workspace graph-warm result, projected from a
/// `project_workspace_graph` row (the durable freshness source of truth).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceWarmStatusEntry {
    /// Workspace slug (matches the `code_graph workspaces` op).
    pub workspace_slug: String,
    /// Persisted warm status: `ready | warming | failed | timed_out`.
    /// `failed`/`timed_out` mean the indexer produced no SCIP artifact for
    /// this workspace, even if the overall project graph warmed from other
    /// workspaces.
    pub status: String,
    /// Commit SHA the row was warmed at, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    /// ISO-8601 UTC timestamp of the last warm attempt for this workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warmed_at: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct RetriggerImageBuildResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Tools ────────────────────────────────────────────────────────────────────

#[tool_router(router = indexer_tool_router, vis = "pub(super)")]
impl DjinnMcpServer {
    #[tool(
        description = "Get the `code_graph` noise filter for a project: `graph_excluded_paths` (globs dropped from cycles/orphans/ranked/impact/edges) and `graph_orphan_ignore` (exact paths the `orphans` op drops)."
    )]
    pub async fn project_graph_exclusions_get(
        &self,
        Parameters(input): Parameters<ProjectGraphExclusionsGetParams>,
    ) -> Json<ProjectGraphExclusionsResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project = match resolve_project(&repo, &input.project).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Json(ProjectGraphExclusionsResponse {
                    status: format!("error: project not found: {}", input.project),
                    project: input.project,
                    graph_excluded_paths: Vec::new(),
                    graph_orphan_ignore: Vec::new(),
                });
            }
            Err(e) => {
                return Json(ProjectGraphExclusionsResponse {
                    status: format!("error: {e}"),
                    project: input.project,
                    graph_excluded_paths: Vec::new(),
                    graph_orphan_ignore: Vec::new(),
                });
            }
        };
        let slug = project.slug();
        match repo.get_config(&project.id).await {
            Ok(Some(config)) => Json(ProjectGraphExclusionsResponse {
                status: "ok".into(),
                project: slug,
                graph_excluded_paths: config.graph_excluded_paths,
                graph_orphan_ignore: config.graph_orphan_ignore,
            }),
            Ok(None) => Json(ProjectGraphExclusionsResponse {
                status: "ok".into(),
                project: slug,
                graph_excluded_paths: Vec::new(),
                graph_orphan_ignore: Vec::new(),
            }),
            Err(e) => Json(ProjectGraphExclusionsResponse {
                status: format!("error: {e}"),
                project: slug,
                graph_excluded_paths: Vec::new(),
                graph_orphan_ignore: Vec::new(),
            }),
        }
    }

    #[tool(
        description = "Set the `code_graph` noise filter for a project. Either field may be omitted; only supplied lists are rewritten. Changing the filter is zero-cost — the canonical warmer cache is untouched and the filter applies at query time."
    )]
    pub async fn project_graph_exclusions_set(
        &self,
        Parameters(input): Parameters<ProjectGraphExclusionsSetParams>,
    ) -> Json<ProjectGraphExclusionsResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project = match resolve_project(&repo, &input.project).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Json(ProjectGraphExclusionsResponse {
                    status: format!("error: project not found: {}", input.project),
                    project: input.project,
                    graph_excluded_paths: Vec::new(),
                    graph_orphan_ignore: Vec::new(),
                });
            }
            Err(e) => {
                return Json(ProjectGraphExclusionsResponse {
                    status: format!("error: {e}"),
                    project: input.project,
                    graph_excluded_paths: Vec::new(),
                    graph_orphan_ignore: Vec::new(),
                });
            }
        };
        let slug = project.slug();

        // Issue one update_config_field per supplied list. The repo
        // method already validates the JSON round-trip so bad input
        // surfaces as an error rather than a corrupt write.
        if let Some(globs) = &input.graph_excluded_paths {
            let serialized = match serde_json::to_string(globs) {
                Ok(s) => s,
                Err(e) => {
                    return Json(ProjectGraphExclusionsResponse {
                        status: format!("error: encode graph_excluded_paths: {e}"),
                        project: slug,
                        graph_excluded_paths: Vec::new(),
                        graph_orphan_ignore: Vec::new(),
                    });
                }
            };
            if let Err(e) = repo
                .update_config_field(&project.id, "graph_excluded_paths", &serialized)
                .await
            {
                return Json(ProjectGraphExclusionsResponse {
                    status: format!("error: {e}"),
                    project: slug,
                    graph_excluded_paths: Vec::new(),
                    graph_orphan_ignore: Vec::new(),
                });
            }
        }
        if let Some(orphans) = &input.graph_orphan_ignore {
            let serialized = match serde_json::to_string(orphans) {
                Ok(s) => s,
                Err(e) => {
                    return Json(ProjectGraphExclusionsResponse {
                        status: format!("error: encode graph_orphan_ignore: {e}"),
                        project: slug,
                        graph_excluded_paths: Vec::new(),
                        graph_orphan_ignore: Vec::new(),
                    });
                }
            };
            if let Err(e) = repo
                .update_config_field(&project.id, "graph_orphan_ignore", &serialized)
                .await
            {
                return Json(ProjectGraphExclusionsResponse {
                    status: format!("error: {e}"),
                    project: slug,
                    graph_excluded_paths: Vec::new(),
                    graph_orphan_ignore: Vec::new(),
                });
            }
        }

        match repo.get_config(&project.id).await {
            Ok(Some(config)) => Json(ProjectGraphExclusionsResponse {
                status: "ok".into(),
                project: slug,
                graph_excluded_paths: config.graph_excluded_paths,
                graph_orphan_ignore: config.graph_orphan_ignore,
            }),
            Ok(None) => Json(ProjectGraphExclusionsResponse {
                status: "ok".into(),
                project: slug,
                graph_excluded_paths: Vec::new(),
                graph_orphan_ignore: Vec::new(),
            }),
            Err(e) => Json(ProjectGraphExclusionsResponse {
                status: format!("error: {e}"),
                project: slug,
                graph_excluded_paths: Vec::new(),
                graph_orphan_ignore: Vec::new(),
            }),
        }
    }

    /// Return the detected stack metadata for a project, as populated by
    /// the mirror-fetcher hook after each successful fetch. The empty JSON
    /// default (`{}`) surfaces as `stack: None`.
    #[tool(
        description = "Return detected stack metadata for a project (languages, package managers, frameworks, devcontainer status)."
    )]
    pub async fn get_project_stack(
        &self,
        Parameters(input): Parameters<GetProjectStackParams>,
    ) -> Json<GetProjectStackResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.get_stack(&input.project).await {
            Ok(Some(raw)) => {
                if raw.trim() == "{}" || raw.trim().is_empty() {
                    return Json(GetProjectStackResponse {
                        stack: None,
                        error: None,
                    });
                }
                match serde_json::from_str::<Stack>(&raw) {
                    Ok(stack) => Json(GetProjectStackResponse {
                        stack: Some(stack),
                        error: None,
                    }),
                    Err(err) => Json(GetProjectStackResponse {
                        stack: None,
                        error: Some(format!("stack JSON deserialize failed: {err}")),
                    }),
                }
            }
            Ok(None) => Json(GetProjectStackResponse {
                stack: None,
                error: Some(format!("project not found: {}", input.project)),
            }),
            Err(err) => Json(GetProjectStackResponse {
                stack: None,
                error: Some(format!("stack lookup failed: {err}")),
            }),
        }
    }

    /// Return image-build status for a project.
    ///
    /// Drives the UI status badge (P7). Joins the image state with derived
    /// graph freshness so the badge can reflect both build progress and
    /// canonical-graph warm readiness.
    #[tool(
        description = "Return image-build status for a project (image_tag, image_status, image_last_error, graph_warmed_at, graph_warm_status). Drives the UI status badge."
    )]
    pub async fn get_project_devcontainer_status(
        &self,
        Parameters(input): Parameters<GetProjectDevcontainerStatusParams>,
    ) -> Json<GetProjectDevcontainerStatusResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        // Existence check — if the project id is unknown, surface it up
        // front so the badge doesn't render "building…" indefinitely.
        if let Err(err) = repo.get_stack(&input.project).await {
            return Json(error_response(
                ProjectImageStatus::NONE.to_string(),
                None,
                format!("stack lookup failed: {err}"),
            ));
        }

        // Catalog-aware (migration 46): the badge reflects the project's
        // assigned catalog image, not the legacy per-project columns. A
        // project with no catalog image resolves to `none` + `needs_image`.
        let dispatch = match repo.resolve_dispatch_image(&input.project).await {
            Ok(Some(d)) => d,
            Ok(None) => {
                return Json(error_response(
                    ProjectImageStatus::NONE.to_string(),
                    None,
                    "project not found".to_string(),
                ));
            }
            Err(err) => {
                return Json(error_response(
                    ProjectImageStatus::NONE.to_string(),
                    None,
                    format!("image state lookup failed: {err}"),
                ));
            }
        };
        let needs_image = dispatch.from_catalog.is_none();
        // Prefer the digest-pinned ref the runtime actually uses, else the tag.
        let image_tag = dispatch.pull_ref().or(dispatch.tag);

        // Graph-warm status: derived from per-workspace freshness first, then
        // the merged repo graph cache. There is no project-table freshness
        // scalar fallback after the storage migration; a cache-only project must remain visibly warmed during rollout.
        let graph_warmed_at =
            graph_warmed_at_from_freshness(self.state.db().clone(), &input.project).await;

        let workspace_warm_statuses =
            workspace_warm_statuses_from_db(self.state.db().clone(), &input.project).await;
        let graph_warm_status = derive_graph_warm_status_with_workspaces(
            &dispatch.status,
            &graph_warmed_at,
            &workspace_warm_statuses,
        );

        Json(GetProjectDevcontainerStatusResponse {
            image_tag,
            image_status: dispatch.status,
            needs_image,
            image_last_error: dispatch.last_error,
            graph_warmed_at,
            graph_warm_status,
            workspace_warm_statuses,
            error: None,
        })
    }

    /// Force the image controller to rebuild the project's image on the
    /// next mirror-fetch tick.
    ///
    /// Nulls `projects.image_hash` so the controller's unchanged-hash
    /// fast-path is defeated; the next `enqueue(project_id, stack)` call
    /// recomputes from HEAD and submits a build Job. Status is flipped to
    /// `building` so the banner reflects the pending rebuild immediately
    /// without waiting for the mirror-fetcher cadence.
    #[tool(
        description = "Mark a project's image for rebuild on the next mirror-fetch tick. Nulls the cached devcontainer hash so the image controller re-enqueues a build."
    )]
    pub async fn retrigger_image_build(
        &self,
        Parameters(input): Parameters<RetriggerImageBuildParams>,
    ) -> Json<RetriggerImageBuildResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        // Load the current image record so we don't clobber tag / error.
        let mut image = match repo.get_project_image(&input.project).await {
            Ok(Some(img)) => img,
            Ok(None) => {
                return Json(RetriggerImageBuildResponse {
                    status: "error".into(),
                    error: Some(format!("project not found: {}", input.project)),
                });
            }
            Err(err) => {
                return Json(RetriggerImageBuildResponse {
                    status: "error".into(),
                    error: Some(format!("image state lookup failed: {err}")),
                });
            }
        };

        // Clear hash + error, flip to building. The controller's next
        // enqueue recomputes the hash from HEAD and submits a Job.
        image.hash = None;
        image.last_error = None;
        image.status = ProjectImageStatus::BUILDING.to_string();

        match repo.set_project_image(&input.project, &image).await {
            Ok(()) => Json(RetriggerImageBuildResponse {
                status: "ok".into(),
                error: None,
            }),
            Err(err) => Json(RetriggerImageBuildResponse {
                status: "error".into(),
                error: Some(format!("failed to flag image for rebuild: {err}")),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::events::EventBus;
    use djinn_db::{ProjectRepository, ProjectWorkspaceGraphUpsert, RepoGraphCacheInsert};

    #[tokio::test]
    async fn graph_freshness_badge_uses_cache_when_workspace_missing() {
        let db = Database::open_in_memory().expect("in-memory db");
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let cache_repo = RepoGraphCacheRepository::new(db.clone());
        let project = project_repo
            .create("badge-cache", "acme", "badge-cache")
            .await
            .expect("create project");

        cache_repo
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: "cache-sha",
                graph_blob: b"graph",
            })
            .await
            .expect("cache upsert");
        let cache_stamp = cache_repo
            .latest_for_project(&project.id)
            .await
            .expect("cache latest")
            .expect("cache row")
            .built_at;

        assert_eq!(
            graph_warmed_at_from_freshness(db, &project.id).await,
            Some(cache_stamp)
        );
    }

    #[tokio::test]
    async fn graph_freshness_badge_prefers_latest_workspace_stamp() {
        let db = Database::open_in_memory().expect("in-memory db");
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let workspace_repo = ProjectWorkspaceGraphRepository::new(db.clone());
        let cache_repo = RepoGraphCacheRepository::new(db.clone());
        let project = project_repo
            .create("badge-workspace", "acme", "badge-workspace")
            .await
            .expect("create project");

        cache_repo
            .upsert(RepoGraphCacheInsert {
                project_id: &project.id,
                commit_sha: "cache-sha",
                graph_blob: b"graph",
            })
            .await
            .expect("cache upsert");
        workspace_repo
            .upsert(ProjectWorkspaceGraphUpsert {
                project_id: &project.id,
                workspace_slug: "old",
                commit_sha: "old-sha",
                status: "ready",
            })
            .await
            .expect("old workspace upsert");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        workspace_repo
            .upsert(ProjectWorkspaceGraphUpsert {
                project_id: &project.id,
                workspace_slug: "new",
                commit_sha: "new-sha",
                status: "ready",
            })
            .await
            .expect("new workspace upsert");
        let workspace_stamp = workspace_repo
            .latest_warmed_state(&project.id)
            .await
            .expect("workspace latest")
            .expect("workspace row")
            .warmed_at;

        assert_eq!(
            graph_warmed_at_from_freshness(db, &project.id).await,
            Some(workspace_stamp)
        );
    }

    fn warm_entry(workspace_slug: &str, status: &str) -> WorkspaceWarmStatusEntry {
        WorkspaceWarmStatusEntry {
            workspace_slug: workspace_slug.to_string(),
            status: status.to_string(),
            commit_sha: Some("abc123".to_string()),
            warmed_at: Some("2026-06-12T00:00:00Z".to_string()),
        }
    }

    // The live bug: `server` warms `timed_out` while other workspaces warm
    // fine. Because the overall graph IS warmed from those other
    // workspaces (`graph_warmed_at` is set), the aggregate must surface
    // `degraded` — usable but incomplete — rather than masking it as
    // `ready` or over-alarming as `failed`. `failed`/`timed_out` is
    // evaluated first so it still wins over a co-existing shrink
    // `warning`, mirroring the precedence in
    // `canonical_graph::detect_graph_cache_shrink_warning` (clky).
    #[test]
    fn graph_warm_status_degraded_when_failed_and_graph_warmed() {
        let statuses = vec![
            warm_entry("ui", "ready"),
            warm_entry("graph-cache", "warning"),
            warm_entry("server", "failed"),
        ];

        assert_eq!(
            derive_graph_warm_status_with_workspaces(
                ProjectImageStatus::READY,
                &Some("2026-06-12T00:00:00Z".to_string()),
                &statuses,
            ),
            "degraded"
        );
    }

    // `timed_out` follows the same rule as `failed`.
    #[test]
    fn graph_warm_status_degraded_when_timed_out_and_graph_warmed() {
        let statuses = vec![
            warm_entry("ui", "ready"),
            warm_entry("graph-cache", "warning"),
            warm_entry("server", "timed_out"),
        ];

        assert_eq!(
            derive_graph_warm_status_with_workspaces(
                ProjectImageStatus::READY,
                &Some("2026-06-12T00:00:00Z".to_string()),
                &statuses,
            ),
            "degraded"
        );
    }

    // When NOTHING warmed at all (no `graph_warmed_at`) but a workspace
    // failed/timed_out, it is a hard `failed` — there is no usable graph
    // to call `degraded`.
    #[test]
    fn graph_warm_status_failed_when_failed_and_nothing_warmed() {
        let statuses = vec![warm_entry("server", "timed_out")];

        assert_eq!(
            derive_graph_warm_status_with_workspaces(ProjectImageStatus::READY, &None, &statuses,),
            "failed"
        );
    }

    // A non-fatal graph-cache shrink warning beats the regular
    // ready/running precedence when no failed/timed_out workspaces are
    // present. `graph_warmed_at` is preserved (the cache is fresh) but the
    // derived status flips to `warning`.
    #[test]
    fn graph_warm_status_warning_beats_ready_when_no_failures() {
        let statuses = vec![
            warm_entry("ui", "ready"),
            warm_entry("graph-cache", "warning"),
        ];

        assert_eq!(
            derive_graph_warm_status_with_workspaces(
                ProjectImageStatus::READY,
                &Some("2026-06-12T00:00:00Z".to_string()),
                &statuses,
            ),
            "warning"
        );
    }

    // The warning must still surface when the image is in `running`
    // state — the running/runnable projection should not mask a
    // shrink warning.
    #[test]
    fn graph_warm_status_warning_beats_running_when_no_failures() {
        let statuses = vec![warm_entry("graph-cache", "warning")];

        assert_eq!(
            derive_graph_warm_status_with_workspaces(ProjectImageStatus::READY, &None, &statuses,),
            "warning"
        );
    }

    // Sanity: the existing normal-ready/running behavior must remain
    // unchanged when every workspace is healthy.
    #[test]
    fn graph_warm_status_ready_when_warmed_and_workspaces_ready() {
        let statuses = vec![warm_entry("server", "ready")];

        assert_eq!(
            derive_graph_warm_status_with_workspaces(
                ProjectImageStatus::READY,
                &Some("2026-06-12T00:00:00Z".to_string()),
                &statuses,
            ),
            "ready"
        );
    }

    #[test]
    fn graph_warm_status_running_when_image_ready_but_not_yet_warmed() {
        let statuses = vec![warm_entry("server", "ready")];

        assert_eq!(
            derive_graph_warm_status_with_workspaces(ProjectImageStatus::READY, &None, &statuses,),
            "running"
        );
    }

    #[test]
    fn graph_warm_status_pending_when_nothing_warmed_and_no_workspaces() {
        assert_eq!(
            derive_graph_warm_status_with_workspaces(ProjectImageStatus::NONE, &None, &[],),
            "pending"
        );
    }

    // End-to-end handler read path (the live djinnos/djinn bug): a
    // `timed_out` row for `server` alongside a healthy `ui` row must yield
    // a POPULATED `workspace_warm_statuses` list AND a `degraded`
    // aggregate — the previous filesystem source returned an empty list
    // and a `ready` aggregate, hiding the failure. The `__djinn_no_code__`
    // sentinel is filtered out, matching the `workspaces` op.
    #[tokio::test]
    async fn workspace_warm_statuses_from_db_surfaces_timed_out_and_degrades_aggregate() {
        let db = Database::open_in_memory().expect("in-memory db");
        let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
        let workspace_repo = ProjectWorkspaceGraphRepository::new(db.clone());
        let project = project_repo
            .create("degraded-project", "djinnos", "djinn")
            .await
            .expect("create project");

        workspace_repo
            .upsert(ProjectWorkspaceGraphUpsert {
                project_id: &project.id,
                workspace_slug: "ui",
                commit_sha: "sha-ui",
                status: "ready",
            })
            .await
            .expect("ui upsert");
        workspace_repo
            .upsert(ProjectWorkspaceGraphUpsert {
                project_id: &project.id,
                workspace_slug: "server",
                commit_sha: "sha-server",
                status: "timed_out",
            })
            .await
            .expect("server upsert");
        // The code-less sentinel must NOT appear in the surfaced list.
        workspace_repo
            .upsert(ProjectWorkspaceGraphUpsert {
                project_id: &project.id,
                workspace_slug: djinn_db::CODELESS_WORKSPACE_SLUG,
                commit_sha: "sha-none",
                status: "ready",
            })
            .await
            .expect("sentinel upsert");

        let statuses = workspace_warm_statuses_from_db(db.clone(), &project.id).await;
        assert_eq!(
            statuses.len(),
            2,
            "sentinel filtered out; ui + server surfaced"
        );
        let server = statuses
            .iter()
            .find(|entry| entry.workspace_slug == "server")
            .expect("server row surfaced");
        assert_eq!(server.status, "timed_out");
        assert_eq!(server.commit_sha.as_deref(), Some("sha-server"));
        assert!(server.warmed_at.is_some(), "warmed_at populated from row");
        assert!(
            statuses
                .iter()
                .all(|e| e.workspace_slug != djinn_db::CODELESS_WORKSPACE_SLUG),
            "code-less sentinel excluded"
        );

        // The overall graph warmed from `ui`, so the aggregate is the
        // partial-success `degraded`, not `failed` and not a masked `ready`.
        let warmed_at = graph_warmed_at_from_freshness(db, &project.id).await;
        assert!(warmed_at.is_some(), "ui warm produced a freshness stamp");
        assert_eq!(
            derive_graph_warm_status_with_workspaces(
                ProjectImageStatus::READY,
                &warmed_at,
                &statuses,
            ),
            "degraded"
        );
    }

    #[test]
    fn workspace_warm_status_entry_round_trips() {
        let entries = vec![
            WorkspaceWarmStatusEntry {
                workspace_slug: "ui".to_string(),
                status: "ready".to_string(),
                commit_sha: Some("sha-ui".to_string()),
                warmed_at: Some("2026-06-12T00:00:00Z".to_string()),
            },
            WorkspaceWarmStatusEntry {
                workspace_slug: "server".to_string(),
                status: "timed_out".to_string(),
                commit_sha: Some("sha-server".to_string()),
                warmed_at: Some("2026-06-12T00:00:01Z".to_string()),
            },
        ];

        let serialized = serde_json::to_string(&entries).expect("serialize");
        let parsed: Vec<WorkspaceWarmStatusEntry> =
            serde_json::from_str(&serialized).expect("deserialize");

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].workspace_slug, "ui");
        assert_eq!(parsed[0].status, "ready");
        assert_eq!(parsed[1].workspace_slug, "server");
        assert_eq!(parsed[1].status, "timed_out");
        assert_eq!(parsed[1].commit_sha.as_deref(), Some("sha-server"));
    }
}
