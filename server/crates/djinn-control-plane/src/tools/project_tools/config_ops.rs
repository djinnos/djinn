use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::lifecycle_ops::resolve_project;
use crate::server::DjinnMcpServer;
use crate::tools::ObjectJson;
use djinn_db::ProjectRepository;

/// Build a success-shape `ProjectConfigResponse` from a fully-populated
/// [`ProjectConfig`].
fn project_config_ok(
    project: &djinn_core::models::Project,
    config: djinn_db::ProjectConfig,
) -> ProjectConfigResponse {
    ProjectConfigResponse {
        status: "ok".into(),
        project: project.slug(),
        target_branch: config.target_branch,
        auto_merge: config.auto_merge,
        sync_enabled: config.sync_enabled,
        sync_remote: config.sync_remote,
        graph_excluded_paths: config.graph_excluded_paths,
        graph_orphan_ignore: config.graph_orphan_ignore,
    }
}

/// Fallback shape used when `get_config` returns `None` (no row) or
/// an error — we still echo back the denormalized fields from the
/// `Project` row itself.
fn project_config_fallback(
    status: String,
    project: &djinn_core::models::Project,
) -> ProjectConfigResponse {
    ProjectConfigResponse {
        status,
        project: project.slug(),
        target_branch: project.target_branch.clone(),
        auto_merge: project.auto_merge,
        sync_enabled: project.sync_enabled,
        sync_remote: project.sync_remote.clone(),
        graph_excluded_paths: Vec::new(),
        graph_orphan_ignore: Vec::new(),
    }
}

/// Error shape used when the project lookup itself fails, so we don't
/// even have a `Project` to echo.
fn project_config_error(project_ref: &str, status: String) -> ProjectConfigResponse {
    ProjectConfigResponse {
        status,
        project: project_ref.to_owned(),
        target_branch: "main".into(),
        auto_merge: true,
        sync_enabled: false,
        sync_remote: None,
        graph_excluded_paths: Vec::new(),
        graph_orphan_ignore: Vec::new(),
    }
}

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
pub struct ProjectConfigGetParams {
    pub project: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectConfigSetParams {
    pub project: String,
    pub key: String,
    pub value: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectEnvironmentConfigGetParams {
    /// Project UUID.
    pub project: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectEnvironmentConfigSetParams {
    /// Project UUID.
    pub project: String,
    /// Full `EnvironmentConfig` JSON blob. Validated server-side via
    /// `djinn_stack::environment::EnvironmentConfig::validate` before
    /// anything is written.
    #[schemars(with = "djinn_stack::environment::EnvironmentConfig")]
    pub config: ObjectJson,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectEnvironmentConfigResetParams {
    /// Project UUID.
    pub project: String,
}

// ── Response structs ─────────────────────────────────────────────────────────

#[derive(Serialize, JsonSchema)]
pub struct ProjectConfigResponse {
    pub status: String,
    pub project: String,
    pub target_branch: String,
    pub auto_merge: bool,
    pub sync_enabled: bool,
    pub sync_remote: Option<String>,
    /// Glob patterns the `code_graph` MCP handler drops from
    /// cycles/orphans/ranked result sets (migration 12). Canonical empty
    /// value is an empty array, not null, so the UI can bind a list
    /// editor to it without a pre-fetch fallback.
    #[serde(default)]
    pub graph_excluded_paths: Vec<String>,
    /// Exact file paths the `code_graph orphans` op silently drops
    /// (migration 12). Intended for the Dead-code panel's "mark not
    /// actually dead" affordance.
    #[serde(default)]
    pub graph_orphan_ignore: Vec<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectEnvironmentConfigGetResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The raw JSON config currently in `projects.environment_config`.
    /// Empty object `{}` when the row hasn't been reseeded yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<djinn_stack::environment::EnvironmentConfig>")]
    pub config: Option<ObjectJson>,
    /// The catalog image this project is assigned to, if any (for the picker).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_image_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_image_name: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectEnvironmentConfigSetResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectEnvironmentConfigResetResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The freshly-generated auto-detected config, on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<djinn_stack::environment::EnvironmentConfig>")]
    pub config: Option<ObjectJson>,
}

// ── Tools ────────────────────────────────────────────────────────────────────

#[tool_router(router = config_tool_router, vis = "pub(super)")]
impl DjinnMcpServer {
    #[tool(description = "Get project config fields for a project path.")]
    pub async fn project_config_get(
        &self,
        Parameters(input): Parameters<ProjectConfigGetParams>,
    ) -> Json<ProjectConfigResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project = match resolve_project(&repo, &input.project).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Json(project_config_error(
                    &input.project,
                    format!("error: project not found: {}", input.project),
                ));
            }
            Err(e) => {
                return Json(project_config_error(&input.project, format!("error: {e}")));
            }
        };
        match repo.get_config(&project.id).await {
            Ok(Some(config)) => Json(project_config_ok(&project, config)),
            Ok(None) => Json(project_config_fallback("ok".into(), &project)),
            Err(e) => Json(project_config_fallback(format!("error: {e}"), &project)),
        }
    }

    #[tool(description = "Set a single project config field by key.")]
    pub async fn project_config_set(
        &self,
        Parameters(input): Parameters<ProjectConfigSetParams>,
    ) -> Json<ProjectConfigResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project = match resolve_project(&repo, &input.project).await {
            Ok(Some(project)) => project,
            Ok(None) => {
                return Json(project_config_error(
                    &input.project,
                    format!("error: project not found: {}", input.project),
                ));
            }
            Err(e) => {
                return Json(project_config_error(&input.project, format!("error: {e}")));
            }
        };

        match repo
            .update_config_field(&project.id, &input.key, &input.value)
            .await
        {
            Ok(Some(config)) => Json(project_config_ok(&project, config)),
            Ok(None) => Json(project_config_fallback(
                format!("error: invalid key '{}'", input.key),
                &project,
            )),
            Err(e) => Json(project_config_fallback(format!("error: {e}"), &project)),
        }
    }

    /// Return the current `environment_config` JSON for a project.
    ///
    /// Returns `{}` while the boot reseed hook hasn't seen the row yet
    /// — callers can treat that as "show the auto-detection preview"
    /// or surface a "not seeded yet" state.
    #[tool(
        description = "Read projects.environment_config as JSON. Returns '{}' for projects that haven't been reseeded yet."
    )]
    pub async fn project_environment_config_get(
        &self,
        Parameters(input): Parameters<ProjectEnvironmentConfigGetParams>,
    ) -> Json<ProjectEnvironmentConfigGetResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.get_environment_config(&input.project).await {
            Ok(Some(raw)) => {
                let parsed = serde_json::from_str::<serde_json::Value>(&raw)
                    .unwrap_or(serde_json::json!({}));
                // Surface the assigned catalog image so the UI picker can
                // pre-select it by name.
                let selected = djinn_db::ImageRepository::new(self.state.db().clone())
                    .resolve_for_project(&input.project)
                    .await
                    .ok()
                    .flatten();
                Json(ProjectEnvironmentConfigGetResponse {
                    status: "ok".into(),
                    error: None,
                    config: Some(ObjectJson::from(parsed)),
                    selected_image_id: selected.as_ref().map(|i| i.id.clone()),
                    selected_image_name: selected.map(|i| i.name),
                })
            }
            Ok(None) => Json(ProjectEnvironmentConfigGetResponse {
                status: "error".into(),
                error: Some(format!("project not found: {}", input.project)),
                config: None,
                selected_image_id: None,
                selected_image_name: None,
            }),
            Err(err) => Json(ProjectEnvironmentConfigGetResponse {
                status: "error".into(),
                error: Some(format!("db error: {err}")),
                config: None,
                selected_image_id: None,
                selected_image_name: None,
            }),
        }
    }

    /// Write a validated `environment_config` JSON blob for a project.
    ///
    /// Flow: validate → upsert the runtime ConfigMap (so warm/task-run
    /// Pods scheduled after this call see the new config) → write to
    /// Dolt (which nulls `image_hash` so the next mirror-fetch tick
    /// rebuilds the image).
    #[tool(
        description = "Validate + persist projects.environment_config, upsert the runtime ConfigMap, and null image_hash so the next tick rebuilds the image. Accepts a JSON EnvironmentConfig."
    )]
    pub async fn project_environment_config_set(
        &self,
        Parameters(input): Parameters<ProjectEnvironmentConfigSetParams>,
    ) -> Json<ProjectEnvironmentConfigSetResponse> {
        // Parse + validate up front so the MCP error surface is the
        // typed EnvironmentConfigError, not whatever the DB layer
        // returns later.
        let cfg: djinn_stack::environment::EnvironmentConfig =
            match serde_json::from_value(serde_json::Value::Object(input.config.0)) {
                Ok(c) => c,
                Err(err) => {
                    return Json(ProjectEnvironmentConfigSetResponse {
                        status: "error".into(),
                        error: Some(format!("parse config: {err}")),
                    });
                }
            };
        if let Err(err) = cfg.validate() {
            return Json(ProjectEnvironmentConfigSetResponse {
                status: "error".into(),
                error: Some(format!("validate: {err}")),
            });
        }

        // Mark it as user-edited so the boot reseed hook leaves it
        // alone on the next server restart.
        let mut cfg = cfg;
        cfg.source = djinn_stack::environment::ConfigSource::UserEdited;

        // Dispatch through the RuntimeOps bridge — production apps
        // upsert the runtime ConfigMap via the image-controller; test
        // stubs fall back to a plain DB write.
        if let Err(err) = self
            .state
            .apply_environment_config(&input.project, &cfg)
            .await
        {
            return Json(ProjectEnvironmentConfigSetResponse {
                status: "error".into(),
                error: Some(format!("apply: {err}")),
            });
        }

        Json(ProjectEnvironmentConfigSetResponse {
            status: "ok".into(),
            error: None,
        })
    }

    /// Regenerate `environment_config` from the project's current `stack`
    /// column and persist it. Mirrors the boot reseed hook but runs on
    /// demand — the UI's "Reset from auto-detection" button calls this.
    /// The freshly-generated config is tagged `source: AutoDetected`,
    /// so the next boot reseed will still skip it (schema_version >= 1).
    #[tool(
        description = "Regenerate projects.environment_config from projects.stack, overwriting any user edits. Returns the freshly-generated config. Fails if the stack column is empty (no detection has run yet)."
    )]
    pub async fn project_environment_config_reset(
        &self,
        Parameters(input): Parameters<ProjectEnvironmentConfigResetParams>,
    ) -> Json<ProjectEnvironmentConfigResetResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        let stack_raw = match repo.get_stack(&input.project).await {
            Ok(Some(raw)) => raw,
            Ok(None) => {
                return Json(ProjectEnvironmentConfigResetResponse {
                    status: "error".into(),
                    error: Some(format!("project not found: {}", input.project)),
                    config: None,
                });
            }
            Err(err) => {
                return Json(ProjectEnvironmentConfigResetResponse {
                    status: "error".into(),
                    error: Some(format!("db error: {err}")),
                    config: None,
                });
            }
        };
        let trimmed = stack_raw.trim();
        if trimmed.is_empty() || trimmed == "{}" {
            return Json(ProjectEnvironmentConfigResetResponse {
                status: "error".into(),
                error: Some(
                    "project has no detected stack yet — wait for the next mirror-fetch tick and retry"
                        .into(),
                ),
                config: None,
            });
        }
        let stack: djinn_stack::schema::Stack = match serde_json::from_str(trimmed) {
            Ok(s) => s,
            Err(err) => {
                return Json(ProjectEnvironmentConfigResetResponse {
                    status: "error".into(),
                    error: Some(format!("parse stack: {err}")),
                    config: None,
                });
            }
        };

        let cfg = djinn_stack::environment::EnvironmentConfig::from_stack(&stack);
        if let Err(err) = cfg.validate() {
            return Json(ProjectEnvironmentConfigResetResponse {
                status: "error".into(),
                error: Some(format!("validate: {err}")),
                config: None,
            });
        }

        if let Err(err) = self
            .state
            .apply_environment_config(&input.project, &cfg)
            .await
        {
            return Json(ProjectEnvironmentConfigResetResponse {
                status: "error".into(),
                error: Some(format!("apply: {err}")),
                config: None,
            });
        }

        let json = match serde_json::to_value(&cfg) {
            Ok(serde_json::Value::Object(map)) => {
                Some(ObjectJson::from(serde_json::Value::Object(map)))
            }
            _ => None,
        };
        Json(ProjectEnvironmentConfigResetResponse {
            status: "ok".into(),
            error: None,
            config: json,
        })
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProjectRepository};
    use serde_json::json;

    use crate::bridge::RuntimeOps;
    use crate::server::DjinnMcpServer;
    use crate::state::McpState;
    use crate::state::stubs::{
        StubCoordinatorOps, StubGitOps, StubLspOps, StubRepoGraphOps, StubSlotPoolOps,
    };

    /// RuntimeOps stub that persists the `EnvironmentConfig` passed to
    /// `apply_environment_config` to the underlying test DB. Production
    /// runtimes upsert a Kubernetes ConfigMap and may mirror the JSON into
    /// Dolt; in-process tests just need the JSON write so a subsequent
    /// `project_environment_config_get` round-trip can read it back. The
    /// tool's parse + validate + source-tagging logic is exercised
    /// end-to-end through `dispatch_tool` — the test double captures
    /// exactly what the tool persisted, so any field dropped or mutated
    /// before this call would surface as a failed round-trip assertion.
    struct TestRuntimeOps {
        db: Database,
    }

    #[async_trait::async_trait]
    impl RuntimeOps for TestRuntimeOps {
        async fn apply_settings(
            &self,
            _: &djinn_core::models::DjinnSettings,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn embed_memory_query(
            &self,
            _: &str,
        ) -> Result<Option<crate::bridge::SemanticQueryEmbedding>, String> {
            Ok(None)
        }
        async fn reset_runtime_settings(&self) {}
        async fn persist_model_health_state(&self) {}
        async fn apply_environment_config(
            &self,
            project_id: &str,
            config: &djinn_stack::environment::EnvironmentConfig,
        ) -> Result<(), String> {
            // Serialize the exact `EnvironmentConfig` (including the
            // `UserEdited` source tag applied by the set path) and write
            // it to the DB. This is the production-equivalent
            // persistence side-effect: the next `get` reads this row.
            let json = serde_json::to_string(config).map_err(|e| format!("serialize: {e}"))?;
            let repo = ProjectRepository::new(self.db.clone(), EventBus::noop());
            repo.set_environment_config(project_id, &json)
                .await
                .map_err(|e| format!("set_environment_config: {e}"))?;
            Ok(())
        }
        async fn trigger_mirror_refresh(&self, _: &str) {}
        async fn enqueue_image_build(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn trigger_graph_warm(&self, _: &str) {}
        async fn apply_user_model_change(&self) {}
        async fn teardown_taskrun_job(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
        async fn list_taskrun_jobs(&self) -> Result<Vec<crate::bridge::TaskrunJobRef>, String> {
            Ok(Vec::new())
        }
        async fn cleanup_task_branches(&self, _: &str) {}
    }

    async fn test_server(db: Database) -> DjinnMcpServer {
        let state = McpState::new(
            db.clone(),
            EventBus::noop(),
            djinn_provider::catalog::CatalogService::new(),
            djinn_provider::catalog::HealthTracker::new(),
            Some(Arc::new(StubCoordinatorOps)),
            Some(Arc::new(StubSlotPoolOps)),
            None,
            None,
            Arc::new(StubLspOps),
            Arc::new(TestRuntimeOps { db }),
            Arc::new(StubGitOps),
            Arc::new(StubRepoGraphOps),
        );
        DjinnMcpServer::new(state)
    }

    /// Seed a project in the DB and return its UUID.
    async fn seed_project(db: &Database) -> String {
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.create("test-env-cfg", "test", "test-env-cfg")
            .await
            .expect("create project")
            .id
    }

    /// Persist raw environment_config JSON directly to the DB.
    async fn seed_environment_config(db: &Database, project_id: &str, json: &str) {
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.set_environment_config(project_id, json)
            .await
            .expect("seed env config");
    }

    /// Persist a stack JSON so `project_environment_config_reset` has
    /// something to reset from.
    async fn seed_stack(db: &Database, project_id: &str, stack_json: &str) {
        let repo = ProjectRepository::new(db.clone(), EventBus::noop());
        repo.set_stack(project_id, stack_json)
            .await
            .expect("seed stack");
    }

    /// A complete grouped declaration accepted by the environment-config set
    /// path. Keeping this JSON-shaped intentionally exercises the same serde
    /// boundary as the environment page rather than constructing Rust types.
    fn grouped_final_verification_config() -> serde_json::Value {
        json!({
            "schema_version": 1,
            "lifecycle": {
                "final_verification": {
                    "version": 1,
                    "profile_id": "ci-default",
                    "profile_revision": 1,
                    "command_groups": [
                        { "name": "rust", "commands": [{
                            "check_id": "cargo-test", "executable": "cargo", "argv": ["test"],
                            "working_directory": "server", "timeout_seconds": 300,
                            "descriptor_revision": 1
                        }] },
                        { "name": "web", "commands": [{
                            "check_id": "web-test", "executable": "pnpm", "argv": ["test"],
                            "working_directory": "ui", "timeout_seconds": 300,
                            "descriptor_revision": 1
                        }] }
                    ],
                    "selection_rules": [
                        { "match": ["server/**"], "command_groups": ["rust"] },
                        { "match": ["**"], "command_groups": ["rust", "web"] }
                    ],
                    "required_checks": [], "input_manifest": { "version": 1 }, "hermeticity": {}
                }
            }
        })
    }

    async fn set_environment_config(
        server: &DjinnMcpServer,
        project_id: &str,
        config: serde_json::Value,
    ) -> serde_json::Value {
        server
            .dispatch_tool(
                "project_environment_config_set",
                json!({ "project": project_id, "config": config }),
            )
            .await
            .expect("set dispatch")
    }

    // ── AC1: valid set then get round-trip ───────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_accepts_valid_pretask_and_get_returns_it_unchanged() {
        let db = Database::open_in_memory().expect("open db");
        db.ensure_initialized().await.unwrap();
        let project_id = seed_project(&db).await;
        let server = test_server(db.clone()).await;

        let cfg = json!({
            "schema_version": 1,
            "lifecycle": {
                "pre_task": [
                    {
                        "name": "install-deps",
                        "command": "pip install -e .",
                        "timeout_seconds": 120,
                        "failure_policy": "blocking"
                    },
                    {
                        "name": "seed-db",
                        "command": "python manage.py migrate",
                        "timeout_seconds": 600,
                        "failure_policy": "best_effort"
                    },
                    {
                        "command": "echo unnamed",
                        "timeout_seconds": 30
                    }
                ]
            }
        });

        // ── set: validates + tags source UserEdited ──
        let set_result = server
            .dispatch_tool(
                "project_environment_config_set",
                json!({ "project": project_id, "config": cfg }),
            )
            .await
            .expect("set dispatch");
        assert_eq!(
            set_result.get("status").and_then(|v| v.as_str()),
            Some("ok"),
            "set failed: {}",
            set_result
        );

        // The TestRuntimeOps stub persists the exact
        // `EnvironmentConfig` (including the `UserEdited` source tag
        // applied by the set path) to the test DB, mirroring what the
        // production runtime bridge writes. No manual seeding after the
        // set call — that would mask any field dropped or mutated
        // before `apply_environment_config`.

        // ── get: verifies fields survive ──
        let get_result = server
            .dispatch_tool(
                "project_environment_config_get",
                json!({ "project": project_id }),
            )
            .await
            .expect("get dispatch");
        assert_eq!(
            get_result.get("status").and_then(|v| v.as_str()),
            Some("ok"),
            "get failed: {}",
            get_result
        );

        let returned_cfg = get_result.get("config").expect("config field");
        let pre_task = returned_cfg
            .pointer("/lifecycle/pre_task")
            .expect("lifecycle.pre_task missing")
            .as_array()
            .expect("pre_task not array");

        assert_eq!(pre_task.len(), 3);

        // First entry: all fields explicit.
        assert_eq!(pre_task[0]["name"], "install-deps");
        assert_eq!(pre_task[0]["command"], "pip install -e .");
        assert_eq!(pre_task[0]["timeout_seconds"], 120);
        assert_eq!(pre_task[0]["failure_policy"], "blocking");

        // Second entry: best_effort.
        assert_eq!(pre_task[1]["name"], "seed-db");
        assert_eq!(pre_task[1]["command"], "python manage.py migrate");
        assert_eq!(pre_task[1]["timeout_seconds"], 600);
        assert_eq!(pre_task[1]["failure_policy"], "best_effort");

        // Third entry: unnamed. The raw JSON round-trip through the DB
        // preserves exactly what was submitted; serde defaults only fill
        // on deserialization into EnvironmentConfig, not in raw Value
        // form. So fields omitted by the user remain absent.
        assert!(pre_task[2].get("name").is_none() || pre_task[2]["name"].is_null());
        assert_eq!(pre_task[2]["command"], "echo unnamed");
        assert_eq!(pre_task[2]["timeout_seconds"], 30);
    }

    // ── AC1: invalid pre-task rejected via EnvironmentConfig::validate ───

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_rejects_empty_command() {
        let db = Database::open_in_memory().expect("open db");
        db.ensure_initialized().await.unwrap();
        let project_id = seed_project(&db).await;
        let server = test_server(db.clone()).await;

        let cfg = json!({
            "schema_version": 1,
            "lifecycle": {
                "pre_task": [{ "name": "bad", "command": "", "timeout_seconds": 300 }]
            }
        });
        let result = server
            .dispatch_tool(
                "project_environment_config_set",
                json!({ "project": project_id, "config": cfg }),
            )
            .await
            .expect("dispatch");

        assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("error"));
        let error = result.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            error.contains("validate"),
            "expected validate error, got: {error}"
        );
    }

    // Assert MCP-visible validation strings so the environment-page save path
    // retains the field-specific grouped-plan errors from EnvironmentConfig.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_rejects_grouped_final_verification_with_precise_errors() {
        let db = Database::open_in_memory().expect("open db");
        db.ensure_initialized().await.unwrap();
        let project_id = seed_project(&db).await;
        let server = test_server(db).await;
        let cases: [(fn(&mut serde_json::Value), &str); 3] = [
            (
                |config| {
                    config["lifecycle"]["final_verification"]["selection_rules"][0]["match"] =
                        json!([]);
                },
                "validate: lifecycle.final_verification.selection_rules[0].match: value is empty",
            ),
            (
                |config| {
                    config["lifecycle"]["final_verification"]["selection_rules"][0]["command_groups"] =
                        json!(["missing"]);
                },
                "validate: lifecycle.final_verification.selection_rules[0].command_groups: value \"missing\" contains disallowed characters (allowed: [A-Za-z0-9._-])",
            ),
            (
                |config| {
                    config["lifecycle"]["final_verification"]["selection_rules"][1]["match"] =
                        json!(["ui/**"]);
                },
                "validate: lifecycle.final_verification.selection_rules: value is empty",
            ),
        ];

        for (mutate, expected_error) in cases {
            let mut config = grouped_final_verification_config();
            mutate(&mut config);
            let result = set_environment_config(&server, &project_id, config).await;
            assert_eq!(result["status"], "error");
            assert_eq!(result["error"], expected_error);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_get_round_trip_preserves_grouped_final_verification_order() {
        let db = Database::open_in_memory().expect("open db");
        db.ensure_initialized().await.unwrap();
        let project_id = seed_project(&db).await;
        let server = test_server(db).await;

        let set =
            set_environment_config(&server, &project_id, grouped_final_verification_config()).await;
        assert_eq!(set["status"], "ok", "set failed: {set}");
        let get = server
            .dispatch_tool(
                "project_environment_config_get",
                json!({ "project": project_id }),
            )
            .await
            .expect("get dispatch");

        let plan = &get["config"]["lifecycle"]["final_verification"];
        assert_eq!(plan["command_groups"][0]["name"], "rust");
        assert_eq!(plan["command_groups"][1]["name"], "web");
        assert_eq!(plan["selection_rules"][0]["match"], json!(["server/**"]));
        assert_eq!(
            plan["selection_rules"][1]["command_groups"],
            json!(["rust", "web"])
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_get_round_trip_preserves_legacy_final_verification_plan() {
        let db = Database::open_in_memory().expect("open db");
        db.ensure_initialized().await.unwrap();
        let project_id = seed_project(&db).await;
        let server = test_server(db).await;
        let config = json!({
            "schema_version": 1,
            "lifecycle": {
                "final_verification": {
                    "version": 1,
                    "profile_id": "ci-default",
                    "profile_revision": 1,
                    "commands": [{
                        "check_id": "cargo-test", "executable": "cargo", "argv": ["test"],
                        "working_directory": "server", "timeout_seconds": 300,
                        "descriptor_revision": 1
                    }],
                    "required_checks": ["cargo-test"],
                    "input_manifest": { "version": 1 },
                    "hermeticity": {}
                }
            }
        });

        let set = set_environment_config(&server, &project_id, config).await;
        assert_eq!(set["status"], "ok", "set failed: {set}");
        let get = server
            .dispatch_tool(
                "project_environment_config_get",
                json!({ "project": project_id }),
            )
            .await
            .expect("get dispatch");

        let plan = &get["config"]["lifecycle"]["final_verification"];
        assert_eq!(plan["commands"][0]["check_id"], "cargo-test");
        assert!(plan.get("command_groups").is_none());
        assert!(plan.get("selection_rules").is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_rejects_timeout_below_minimum() {
        let db = Database::open_in_memory().expect("open db");
        db.ensure_initialized().await.unwrap();
        let project_id = seed_project(&db).await;
        let server = test_server(db.clone()).await;

        let cfg = json!({
            "schema_version": 1,
            "lifecycle": {
                "pre_task": [{ "name": "bad", "command": "echo oops", "timeout_seconds": 0 }]
            }
        });
        let result = server
            .dispatch_tool(
                "project_environment_config_set",
                json!({ "project": project_id, "config": cfg }),
            )
            .await
            .expect("dispatch");

        assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("error"));
        let error = result.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            error.contains("validate"),
            "expected validate error, got: {error}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_rejects_duplicate_names() {
        let db = Database::open_in_memory().expect("open db");
        db.ensure_initialized().await.unwrap();
        let project_id = seed_project(&db).await;
        let server = test_server(db.clone()).await;

        let cfg = json!({
            "schema_version": 1,
            "lifecycle": {
                "pre_task": [
                    { "name": "dup", "command": "echo a", "timeout_seconds": 60 },
                    { "name": "dup", "command": "echo b", "timeout_seconds": 60 }
                ]
            }
        });
        let result = server
            .dispatch_tool(
                "project_environment_config_set",
                json!({ "project": project_id, "config": cfg }),
            )
            .await
            .expect("dispatch");

        assert_eq!(result.get("status").and_then(|v| v.as_str()), Some("error"));
        let error = result.get("error").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            error.contains("validate"),
            "expected validate error, got: {error}"
        );
    }

    // ── AC3: reset produces empty pre_task defaults ──────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reset_returns_empty_pretask_and_auto_detected_source() {
        let db = Database::open_in_memory().expect("open db");
        db.ensure_initialized().await.unwrap();
        let project_id = seed_project(&db).await;

        // Seed a minimal stack so reset can build a config from it.
        // Stack requires detected_at + all top-level fields.
        seed_stack(
            &db,
            &project_id,
            &json!({
                "detected_at": "2025-01-01T00:00:00Z",
                "languages": [],
                "primary_language": null,
                "package_managers": [],
                "monorepo_tools": [],
                "is_monorepo": false,
                "test_runners": [],
                "frameworks": [],
                "runtimes": { "rust": "stable" },
                "manifest_signals": {
                    "has_package_json": false,
                    "has_cargo_toml": true,
                    "has_pyproject_toml": false,
                    "has_go_mod": false,
                    "has_pnpm_workspace": false,
                    "has_turbo_json": false
                },
                "workspaces": []
            })
            .to_string(),
        )
        .await;

        // Seed a user-edited config with non-empty pre_task entries,
        // so we can verify the reset overwrites them to empty.
        seed_environment_config(
            &db,
            &project_id,
            &json!({
                "schema_version": 1,
                "source": "user_edited",
                "lifecycle": {
                    "pre_task": [
                        { "name": "user-cmd", "command": "echo user", "timeout_seconds": 120 }
                    ]
                }
            })
            .to_string(),
        )
        .await;

        let server = test_server(db.clone()).await;
        let result = server
            .dispatch_tool(
                "project_environment_config_reset",
                json!({ "project": project_id }),
            )
            .await
            .expect("dispatch");

        assert_eq!(
            result.get("status").and_then(|v| v.as_str()),
            Some("ok"),
            "reset failed: {}",
            result
        );

        let returned_cfg = result.get("config").expect("config");
        let pre_task = returned_cfg
            .pointer("/lifecycle/pre_task")
            .expect("pre_task missing in reset config")
            .as_array()
            .expect("pre_task not array");
        assert!(pre_task.is_empty(), "reset should produce empty pre_task");

        assert_eq!(
            returned_cfg.get("source").and_then(|v| v.as_str()),
            Some("auto-detected"),
        );
    }

    // ── AC3: unseeded project returns empty defaults ─────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_returns_empty_defaults_for_unseeded_project() {
        let db = Database::open_in_memory().expect("open db");
        db.ensure_initialized().await.unwrap();
        let project_id = seed_project(&db).await;
        let server = test_server(db.clone()).await;

        let result = server
            .dispatch_tool(
                "project_environment_config_get",
                json!({ "project": project_id }),
            )
            .await
            .expect("dispatch");

        assert_eq!(
            result.get("status").and_then(|v| v.as_str()),
            Some("ok"),
            "get failed: {}",
            result
        );

        let returned_cfg = result.get("config").expect("config");
        assert!(returned_cfg.is_object());

        // lifecycle.pre_task absent or empty — backward-compatible.
        let pre_task = returned_cfg
            .pointer("/lifecycle/pre_task")
            .and_then(|v| v.as_array());
        if let Some(arr) = pre_task {
            assert!(arr.is_empty(), "unseeded pre_task should be empty");
        }
    }

    // ── Source tagging: set marks config as UserEdited ────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_persists_source_as_user_edited() {
        let db = Database::open_in_memory().expect("open db");
        db.ensure_initialized().await.unwrap();
        let project_id = seed_project(&db).await;
        let server = test_server(db.clone()).await;

        let cfg = json!({
            "schema_version": 1,
            "lifecycle": {
                "pre_task": [
                    { "name": "setup", "command": "make setup", "timeout_seconds": 300 }
                ]
            }
        });

        let _ = server
            .dispatch_tool(
                "project_environment_config_set",
                json!({ "project": project_id, "config": cfg }),
            )
            .await
            .expect("dispatch");

        // The TestRuntimeOps stub persists the source-tagged
        // `EnvironmentConfig` (with `UserEdited` → `"user-edited"` via
        // `rename_all = "kebab-case"`) to the test DB. No manual
        // seeding after the set call.
        let get = server
            .dispatch_tool(
                "project_environment_config_get",
                json!({ "project": project_id }),
            )
            .await
            .expect("dispatch");

        let returned_cfg = get.get("config").expect("config");
        assert_eq!(
            returned_cfg.get("source").and_then(|v| v.as_str()),
            Some("user-edited"),
        );
        // Verify the pre_task entry survived.
        let pre_task = returned_cfg
            .pointer("/lifecycle/pre_task")
            .expect("pre_task missing")
            .as_array()
            .expect("not array");
        assert_eq!(pre_task.len(), 1);
        assert_eq!(pre_task[0]["name"], "setup");
        assert_eq!(pre_task[0]["command"], "make setup");
    }

    // ── Round-trip with all failure_policy variants ──────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_get_round_trip_preserves_failure_policy_variants() {
        let db = Database::open_in_memory().expect("open db");
        db.ensure_initialized().await.unwrap();
        let project_id = seed_project(&db).await;
        let server = test_server(db.clone()).await;

        let cfg = json!({
            "schema_version": 1,
            "lifecycle": {
                "pre_task": [
                    {
                        "name": "blocker",
                        "command": "cargo build",
                        "timeout_seconds": 900,
                        "failure_policy": "blocking"
                    },
                    {
                        "name": "optional",
                        "command": "cargo clippy",
                        "timeout_seconds": 300,
                        "failure_policy": "best_effort"
                    }
                ]
            }
        });

        let set_result = server
            .dispatch_tool(
                "project_environment_config_set",
                json!({ "project": project_id, "config": cfg }),
            )
            .await
            .expect("dispatch");
        assert_eq!(set_result["status"], "ok", "set failed: {set_result}");

        // The TestRuntimeOps stub persists the exact
        // `EnvironmentConfig` (with the `UserEdited` source tag applied
        // by the set path) to the test DB. No manual seeding after the
        // set call — that would mask any field dropped or mutated
        // before `apply_environment_config`.
        let get = server
            .dispatch_tool(
                "project_environment_config_get",
                json!({ "project": project_id }),
            )
            .await
            .expect("dispatch");

        let pre_task = get["config"]["lifecycle"]["pre_task"]
            .as_array()
            .expect("array");
        assert_eq!(pre_task.len(), 2);
        assert_eq!(pre_task[0]["failure_policy"], "blocking");
        assert_eq!(pre_task[1]["failure_policy"], "best_effort");
        assert_eq!(pre_task[0]["timeout_seconds"], 900);
        assert_eq!(pre_task[1]["timeout_seconds"], 300);
    }
}
