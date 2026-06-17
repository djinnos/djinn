use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::lifecycle_ops::resolve_project;
use crate::bridge::RuntimeDispatchError;
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

#[derive(Deserialize, JsonSchema)]
pub struct ProjectVerificationGetParams {
    /// Project UUID or `owner/repo` slug.
    pub project: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectVerificationSetParams {
    /// Project UUID or `owner/repo` slug.
    pub project: String,
    /// Full replacement set of verification rules (glob `match_pattern` +
    /// shell `commands`). Validated server-side before persisting.
    pub rules: Vec<djinn_stack::environment::VerificationRule>,
    /// Id from a prior `project_verification_test` whose run PASSED, proving
    /// these exact rules work. REQUIRED when `rules` is non-empty (the save is
    /// rejected otherwise, so a broken rule can't disrupt live tasks); an empty
    /// rule set needs no test.
    #[serde(default)]
    pub test_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectVerificationTestParams {
    /// Project UUID or `owner/repo` slug.
    pub project: String,
    /// Candidate verification rules to test (not saved). Their commands run in
    /// the project's image against the default branch.
    pub rules: Vec<djinn_stack::environment::VerificationRule>,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProjectVerificationTestStatusParams {
    /// The `test_id` returned by `project_verification_test`.
    pub test_id: String,
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

#[derive(Serialize, JsonSchema)]
pub struct ProjectVerificationGetResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Verification rules from the `project_verifications` table (migration
    /// 44). Empty when the project has none.
    pub rules: Vec<djinn_stack::environment::VerificationRule>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectVerificationSetResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectVerificationTestResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Poll `project_verification_test_status` with this id for the result,
    /// then pass it to `project_verification_set` once it has passed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_id: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub struct ProjectVerificationTestStatusResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Test-run lifecycle: pending | running | passed | failed | error.
    pub run_status: String,
    /// True once `run_status == "passed"`.
    pub passed: bool,
    /// Per-command results as a JSON array string
    /// (`[{name,command,exit_code,stdout,stderr,duration_ms}]`).
    pub results_json: String,
    /// Populated when the run itself errored (couldn't dispatch/clone/etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_error: Option<String>,
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

    /// Read a project's verification rules from the `project_verifications`
    /// table (migration 44 — verification left `environment_config` so that
    /// editing a verify command no longer rebuilds the image).
    #[tool(
        description = "Read a project's verification rules (glob match_pattern + shell commands). Returns an empty list when none are configured."
    )]
    pub async fn project_verification_get(
        &self,
        Parameters(input): Parameters<ProjectVerificationGetParams>,
    ) -> Json<ProjectVerificationGetResponse> {
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_id = match repo.resolve(&input.project).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return Json(ProjectVerificationGetResponse {
                    status: "error".into(),
                    error: Some(format!("project not found: {}", input.project)),
                    rules: Vec::new(),
                });
            }
            Err(err) => {
                return Json(ProjectVerificationGetResponse {
                    status: "error".into(),
                    error: Some(format!("db error: {err}")),
                    rules: Vec::new(),
                });
            }
        };
        let vrepo = djinn_db::VerificationRepository::new(self.state.db().clone());
        match vrepo.get_rules(&project_id).await {
            Ok(Some(raw)) => match serde_json::from_str(&raw) {
                Ok(rules) => Json(ProjectVerificationGetResponse {
                    status: "ok".into(),
                    error: None,
                    rules,
                }),
                Err(err) => Json(ProjectVerificationGetResponse {
                    status: "error".into(),
                    error: Some(format!("parse rules: {err}")),
                    rules: Vec::new(),
                }),
            },
            Ok(None) => Json(ProjectVerificationGetResponse {
                status: "ok".into(),
                error: None,
                rules: Vec::new(),
            }),
            Err(err) => Json(ProjectVerificationGetResponse {
                status: "error".into(),
                error: Some(format!("db error: {err}")),
                rules: Vec::new(),
            }),
        }
    }

    /// Replace a project's verification rules (full replacement). Validated
    /// server-side, then persisted to the `project_verifications` table. Does
    /// NOT touch the image — verification is decoupled from the image build
    /// (migration 44), so this never triggers a rebuild.
    #[tool(
        description = "Validate + persist a project's verification rules (full replacement). Does not rebuild the image."
    )]
    pub async fn project_verification_set(
        &self,
        Parameters(input): Parameters<ProjectVerificationSetParams>,
    ) -> Json<ProjectVerificationSetResponse> {
        let verification = djinn_stack::environment::Verification { rules: input.rules };
        if let Err(err) = verification.validate() {
            return Json(ProjectVerificationSetResponse {
                status: "error".into(),
                error: Some(format!("validate: {err}")),
            });
        }
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_id = match repo.resolve(&input.project).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return Json(ProjectVerificationSetResponse {
                    status: "error".into(),
                    error: Some(format!("project not found: {}", input.project)),
                });
            }
            Err(err) => {
                return Json(ProjectVerificationSetResponse {
                    status: "error".into(),
                    error: Some(format!("db error: {err}")),
                });
            }
        };

        // Safety gate: a non-empty rule set must be proven to PASS in the
        // project image before it can be saved — a broken rule fails
        // verification on every in-flight task. An empty rule set runs no
        // verification, so it's always safe and skips the gate.
        if !verification.rules.is_empty() {
            let test_id = match input.test_id.as_deref() {
                Some(t) if !t.trim().is_empty() => t,
                _ => {
                    return Json(ProjectVerificationSetResponse {
                        status: "error".into(),
                        error: Some(
                            "verification rules must be tested before saving: call \
                             project_verification_test, then pass its test_id once it has passed"
                                .into(),
                        ),
                    });
                }
            };
            let test_repo = djinn_db::VerificationTestRepository::new(self.state.db().clone());
            let run = match test_repo.get(test_id).await {
                Ok(Some(r)) => r,
                Ok(None) => {
                    return Json(ProjectVerificationSetResponse {
                        status: "error".into(),
                        error: Some(format!("unknown test_id: {test_id}")),
                    });
                }
                Err(err) => {
                    return Json(ProjectVerificationSetResponse {
                        status: "error".into(),
                        error: Some(format!("db error: {err}")),
                    });
                }
            };
            if run.project_id != project_id {
                return Json(ProjectVerificationSetResponse {
                    status: "error".into(),
                    error: Some("test_id belongs to a different project".into()),
                });
            }
            if run.status != djinn_db::VerificationTestStatus::PASSED {
                return Json(ProjectVerificationSetResponse {
                    status: "error".into(),
                    error: Some(format!(
                        "verification test has not passed (status: {}); fix the commands and re-test",
                        run.status
                    )),
                });
            }
            let tested: Vec<djinn_stack::environment::VerificationRule> =
                serde_json::from_str(&run.candidate_rules).unwrap_or_default();
            if tested != verification.rules {
                return Json(ProjectVerificationSetResponse {
                    status: "error".into(),
                    error: Some(
                        "the rules being saved differ from the tested rules; re-test before saving"
                            .into(),
                    ),
                });
            }
        }

        let rules_json = match serde_json::to_string(&verification.rules) {
            Ok(j) => j,
            Err(err) => {
                return Json(ProjectVerificationSetResponse {
                    status: "error".into(),
                    error: Some(format!("serialize rules: {err}")),
                });
            }
        };
        let vrepo = djinn_db::VerificationRepository::new(self.state.db().clone());
        if let Err(err) = vrepo
            .set_rules(&project_id, &rules_json, "user_edited")
            .await
        {
            return Json(ProjectVerificationSetResponse {
                status: "error".into(),
                error: Some(format!("persist: {err}")),
            });
        }
        Json(ProjectVerificationSetResponse {
            status: "ok".into(),
            error: None,
        })
    }

    /// Run a one-shot test of candidate verification rules in the project's
    /// image (against the default branch), so the UI/API can prove they pass
    /// before saving. Returns a `test_id`; poll `project_verification_test_status`.
    #[tool(
        description = "Test candidate verification rules in the project image (one-shot Job): returns a test_id to poll via project_verification_test_status. Required before saving non-empty rules so a broken rule can't disrupt live tasks."
    )]
    pub async fn project_verification_test(
        &self,
        Parameters(input): Parameters<ProjectVerificationTestParams>,
    ) -> Json<ProjectVerificationTestResponse> {
        let verification = djinn_stack::environment::Verification { rules: input.rules };
        if let Err(err) = verification.validate() {
            return Json(ProjectVerificationTestResponse {
                status: "error".into(),
                error: Some(format!("validate: {err}")),
                test_id: None,
            });
        }
        if verification.rules.is_empty() {
            return Json(ProjectVerificationTestResponse {
                status: "error".into(),
                error: Some("no rules to test (an empty rule set can be saved directly)".into()),
                test_id: None,
            });
        }
        let repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_id = match repo.resolve(&input.project).await {
            Ok(Some(id)) => id,
            Ok(None) => {
                return Json(ProjectVerificationTestResponse {
                    status: "error".into(),
                    error: Some(format!("project not found: {}", input.project)),
                    test_id: None,
                });
            }
            Err(err) => {
                return Json(ProjectVerificationTestResponse {
                    status: "error".into(),
                    error: Some(format!("db error: {err}")),
                    test_id: None,
                });
            }
        };
        let canonical = match serde_json::to_string(&verification.rules) {
            Ok(j) => j,
            Err(err) => {
                return Json(ProjectVerificationTestResponse {
                    status: "error".into(),
                    error: Some(format!("serialize rules: {err}")),
                    test_id: None,
                });
            }
        };
        // Non-crypto fingerprint (no sha2 dep) — the save-gate matches on the
        // stored candidate_rules anyway, so collisions are irrelevant.
        let rules_hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            canonical.hash(&mut h);
            format!("{:016x}", h.finish())
        };
        let test_id = uuid::Uuid::now_v7().to_string();
        let test_repo = djinn_db::VerificationTestRepository::new(self.state.db().clone());
        if let Err(err) = test_repo
            .create(&test_id, &project_id, &rules_hash, &canonical)
            .await
        {
            return Json(ProjectVerificationTestResponse {
                status: "error".into(),
                error: Some(format!("create test run: {err}")),
                test_id: None,
            });
        }
        // Dispatch the one-shot Job (project image → clone → run candidate
        // commands → write outcome to the row). On dispatch failure:
        // * `ImageNotReady { transient: true, .. }` — the catalog image is
        //   mid-rebuild; requeue with backoff until it's ready or the
        //   bounded budget elapses. The row stays in `pending` so a
        //   poller keeps waiting and the operator sees a clear
        //   "dispatched, waiting for image" status.
        // * `ImageNotReady { transient: false, .. }` — the project has
        //   no catalog image assigned; this is a permanent config
        //   problem, mark the row errored immediately.
        // * `Backend(_)` — K8s API / runtime failure, mark the row
        //   errored immediately.
        match self
            .state
            .dispatch_verification_test(&test_id, &project_id)
            .await
        {
            Ok(()) => {}
            Err(RuntimeDispatchError::ImageNotReady {
                project_id: pid,
                image_status,
                tag,
                transient,
            }) => {
                if !transient {
                    // Permanent: project has no catalog image assigned.
                    let msg = format!(
                        "project {pid} has no catalog image assigned (image_status: \
                         {image_status}); cannot dispatch verification test"
                    );
                    let _ = test_repo
                        .complete(
                            &test_id,
                            djinn_db::VerificationTestStatus::ERROR,
                            "[]",
                            Some(&msg),
                        )
                        .await;
                    return Json(ProjectVerificationTestResponse {
                        status: "error".into(),
                        error: Some(format!("dispatch: {msg}")),
                        test_id: Some(test_id),
                    });
                }
                // Transient: bounded requeue. Mirror the agent slot
                // actor's behavior — exponential backoff, capped, with
                // an overall budget. The row stays in `pending` so a
                // poller continues to wait. If the bounded budget
                // elapses we mark the row errored with a clear
                // message.
                let started = std::time::Instant::now();
                let mut backoff = std::time::Duration::from_secs(5);
                let max_backoff = std::time::Duration::from_secs(60);
                let budget = std::time::Duration::from_secs(600);
                let mut attempt: u32 = 0;
                let mut last_status = image_status.clone();
                let mut ok = false;
                let mut perm_failure: Option<String> = None;
                while started.elapsed() < budget {
                    attempt += 1;
                    tracing::info!(
                        test_id = %test_id,
                        project_id = %pid,
                        attempt = attempt,
                        backoff_ms = backoff.as_millis() as u64,
                        image_status = %last_status,
                        "verification-test dispatch: image not ready, retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    match self
                        .state
                        .dispatch_verification_test(&test_id, &project_id)
                        .await
                    {
                        Ok(()) => {
                            tracing::info!(
                                test_id = %test_id,
                                project_id = %pid,
                                attempt = attempt,
                                "verification-test dispatch: image became ready, dispatched"
                            );
                            ok = true;
                            break;
                        }
                        Err(RuntimeDispatchError::ImageNotReady {
                            image_status: new_status,
                            transient: true,
                            ..
                        }) => {
                            last_status = new_status;
                        }
                        Err(RuntimeDispatchError::ImageNotReady {
                            image_status: new_status,
                            transient: false,
                            ..
                        }) => {
                            perm_failure = Some(format!(
                                "project {pid} has no catalog image assigned (image_status: \
                                 {new_status}); cannot dispatch verification test"
                            ));
                            break;
                        }
                        Err(RuntimeDispatchError::Backend(e)) => {
                            perm_failure = Some(format!(
                                "verification-test dispatch: runtime backend error after \
                                 {attempt} requeue attempts: {e}"
                            ));
                            break;
                        }
                    }
                    backoff = (backoff * 2).min(max_backoff);
                }
                if ok {
                    // Row stays in `pending`; the Job will write its
                    // outcome to the row as it runs. The poller
                    // continues to observe a `pending` (then
                    // `running`/`passed`/`failed`) status.
                } else {
                    let msg = perm_failure.unwrap_or_else(|| {
                        format!(
                            "verification-test dispatch: project {pid} image not ready after \
                             {}s (last status: {last_status}{}); the catalog image is still \
                             rebuilding; verify manually once it lands",
                            budget.as_secs(),
                            tag.as_deref()
                                .map(|t| format!(", tag: {t}"))
                                .unwrap_or_default()
                        )
                    });
                    let _ = test_repo
                        .complete(
                            &test_id,
                            djinn_db::VerificationTestStatus::ERROR,
                            "[]",
                            Some(&msg),
                        )
                        .await;
                    return Json(ProjectVerificationTestResponse {
                        status: "error".into(),
                        error: Some(format!("dispatch: {msg}")),
                        test_id: Some(test_id),
                    });
                }
            }
            Err(RuntimeDispatchError::Backend(err)) => {
                let _ = test_repo
                    .complete(
                        &test_id,
                        djinn_db::VerificationTestStatus::ERROR,
                        "[]",
                        Some(&err),
                    )
                    .await;
                return Json(ProjectVerificationTestResponse {
                    status: "error".into(),
                    error: Some(format!("dispatch: {err}")),
                    test_id: Some(test_id),
                });
            }
        }
        Json(ProjectVerificationTestResponse {
            status: "ok".into(),
            error: None,
            test_id: Some(test_id),
        })
    }

    /// Poll the outcome of a `project_verification_test` run.
    #[tool(
        description = "Poll a verification test run by test_id: returns run_status (pending|running|passed|failed|error), passed, and per-command results (results_json)."
    )]
    pub async fn project_verification_test_status(
        &self,
        Parameters(input): Parameters<ProjectVerificationTestStatusParams>,
    ) -> Json<ProjectVerificationTestStatusResponse> {
        let test_repo = djinn_db::VerificationTestRepository::new(self.state.db().clone());
        match test_repo.get(&input.test_id).await {
            Ok(Some(run)) => Json(ProjectVerificationTestStatusResponse {
                status: "ok".into(),
                error: None,
                passed: run.status == djinn_db::VerificationTestStatus::PASSED,
                run_status: run.status,
                results_json: run.results,
                run_error: run.error,
            }),
            Ok(None) => Json(ProjectVerificationTestStatusResponse {
                status: "error".into(),
                error: Some(format!("unknown test_id: {}", input.test_id)),
                run_status: String::new(),
                passed: false,
                results_json: "[]".into(),
                run_error: None,
            }),
            Err(err) => Json(ProjectVerificationTestStatusResponse {
                status: "error".into(),
                error: Some(format!("db error: {err}")),
                run_status: String::new(),
                passed: false,
                results_json: "[]".into(),
                run_error: None,
            }),
        }
    }
}
