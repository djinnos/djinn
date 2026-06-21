//! Per-user preferences exposed over MCP.
//!
//! Identity comes from the task-local [`current_user_id`] scope set by the
//! HTTP MCP handler at request authentication — there is no `user_id`
//! parameter on these tools, by design. An unauthenticated caller gets a
//! clear error rather than reading or mutating someone else's settings.

use std::collections::HashMap;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::acting_user;
use djinn_core::models::ModelLanes;
use djinn_db::UserSettingsRepository;

/// Per-user, per-ROLE ordered model selection over the wire. Each lane is an
/// ordered fallback list (highest priority first) of full `provider/model` ids.
/// A task's base role maps to one lane: `plan` (planner/architect/chat),
/// `implement` (worker), `review` (reviewer).
#[derive(Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelLanesPayload {
    /// planner, architect, chat
    #[serde(default)]
    pub plan: Vec<String>,
    /// worker
    #[serde(default)]
    pub implement: Vec<String>,
    /// reviewer
    #[serde(default)]
    pub review: Vec<String>,
}

impl From<ModelLanes> for ModelLanesPayload {
    fn from(l: ModelLanes) -> Self {
        Self {
            plan: l.plan,
            implement: l.implement,
            review: l.review,
        }
    }
}

impl From<ModelLanesPayload> for ModelLanes {
    fn from(p: ModelLanesPayload) -> Self {
        Self {
            plan: p.plan,
            implement: p.implement,
            review: p.review,
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
pub struct UserSettingsGetParams {
    /// Admin-only: act on behalf of this user id (e.g. another user to
    /// configure). Non-admins must omit it.
    #[serde(default)]
    pub target_user_id: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct UserSettingsGetResponse {
    pub ok: bool,
    /// `users.id` of the signed-in caller (echoed so the UI can sanity-check identity).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Auto-approve PRs that are otherwise ready to merge. When true, the
    /// poller POSTs an APPROVE review using this user's GitHub token at the
    /// moment the PR has CI green + no conflicts + no existing approvals.
    /// Defaults to false. Each task's `created_by_user_id` decides whose
    /// toggle applies; background-agent tasks (`created_by_user_id IS NULL`)
    /// are never auto-approved.
    pub auto_approve_prs: bool,
    /// This user's per-ROLE ordered model lanes (each highest priority first),
    /// full `provider/model` ids. All-empty when the user has no explicit
    /// selection (callers then fall back to the global deployment model list).
    pub lanes: ModelLanesPayload,
    /// This user's per-model concurrency caps (`{ "provider/model": cap }`).
    /// The sole admission control at dispatch; empty ⇒ default 1 per model.
    #[schemars(with = "std::collections::HashMap<String, i64>")]
    pub max_sessions: HashMap<String, u32>,
    /// Cross-model ("Thorough") review. When true (the default), a task
    /// dispatched to the reviewer role prefers a model id different from the one
    /// that implemented it. A degenerate single-model selection falls back to
    /// same-model review.
    pub diverse_review: bool,
    pub error: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UserSettingsSetParams {
    /// Enable or disable auto-approve. Omit to keep the current value.
    pub auto_approve_prs: Option<bool>,
    /// Per-ROLE ordered model lanes for THIS user (each highest priority first),
    /// as full `provider/model` ids: `plan` (planner/architect/chat),
    /// `implement` (worker), `review` (reviewer). Each id must be a model on a
    /// provider this user has connected. Pass all-empty lanes to clear the
    /// selection (→ global fallback). Omit to keep the current value.
    pub lanes: Option<ModelLanesPayload>,
    /// Per-model concurrency caps for THIS user (`{ "provider/model": cap }`).
    /// How many sessions of each model may run concurrently for this user — the
    /// sole admission control (no global ceiling). Pass `{}` to clear (→ default
    /// 1 per model). Omit to keep the current value.
    #[schemars(with = "Option<std::collections::HashMap<String, i64>>")]
    pub max_sessions: Option<HashMap<String, u32>>,
    /// Enable or disable cross-model ("Thorough") review for THIS user. When on
    /// (the default), the reviewer prefers a model id different from the
    /// implementer's. Omit to keep the current value.
    pub diverse_review: Option<bool>,
    /// Admin-only: act on behalf of this user id (e.g. another user to
    /// configure). Non-admins must omit it.
    #[serde(default)]
    pub target_user_id: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct UserSettingsSetResponse {
    pub ok: bool,
    pub applied: bool,
    pub auto_approve_prs: Option<bool>,
    /// The resulting per-role model lanes after the patch.
    pub lanes: Option<ModelLanesPayload>,
    /// The resulting per-model concurrency caps after the patch.
    #[schemars(with = "Option<std::collections::HashMap<String, i64>>")]
    pub max_sessions: Option<HashMap<String, u32>>,
    /// The resulting cross-model review toggle after the patch.
    pub diverse_review: Option<bool>,
    pub error: Option<String>,
}

fn missing_session() -> String {
    "sign in with GitHub to manage user settings".to_string()
}

#[tool_router(router = user_settings_tool_router, vis = "pub")]
impl DjinnMcpServer {
    #[tool(
        description = "Get the signed-in user's settings (auto_approve_prs, …). \
        Returns defaults if the user has never set anything. \
        Errors when no authenticated session is present."
    )]
    pub async fn user_settings_get(
        &self,
        Parameters(p): Parameters<UserSettingsGetParams>,
    ) -> Json<UserSettingsGetResponse> {
        let user_id =
            match acting_user::resolve_effective_user(self.state.db(), p.target_user_id.as_deref())
                .await
            {
                Ok(Some(u)) => u,
                Ok(None) => {
                    return Json(UserSettingsGetResponse {
                        ok: false,
                        user_id: None,
                        auto_approve_prs: false,
                        lanes: ModelLanesPayload::default(),
                        max_sessions: HashMap::new(),
                        diverse_review: true,
                        error: Some(missing_session()),
                    });
                }
                Err(e) => {
                    return Json(UserSettingsGetResponse {
                        ok: false,
                        user_id: None,
                        auto_approve_prs: false,
                        lanes: ModelLanesPayload::default(),
                        max_sessions: HashMap::new(),
                        diverse_review: true,
                        error: Some(e),
                    });
                }
            };
        let repo = UserSettingsRepository::new(self.state.db().clone());
        match repo.get_or_default(&user_id).await {
            Ok(s) => Json(UserSettingsGetResponse {
                ok: true,
                user_id: Some(user_id),
                auto_approve_prs: s.auto_approve_prs,
                lanes: s.lanes.unwrap_or_default().into(),
                max_sessions: s.max_sessions.unwrap_or_default(),
                diverse_review: s.diverse_review,
                error: None,
            }),
            Err(e) => Json(UserSettingsGetResponse {
                ok: false,
                user_id: Some(user_id),
                auto_approve_prs: false,
                lanes: ModelLanesPayload::default(),
                max_sessions: HashMap::new(),
                diverse_review: true,
                error: Some(e.to_string()),
            }),
        }
    }

    #[tool(
        description = "Patch the signed-in user's settings. Only provided fields \
        are updated; omitted fields keep their current values. \
        Errors when no authenticated session is present."
    )]
    pub async fn user_settings_set(
        &self,
        Parameters(p): Parameters<UserSettingsSetParams>,
    ) -> Json<UserSettingsSetResponse> {
        let user_id =
            match acting_user::resolve_effective_user(self.state.db(), p.target_user_id.as_deref())
                .await
            {
                Ok(Some(u)) => u,
                Ok(None) => {
                    return Json(UserSettingsSetResponse {
                        ok: false,
                        applied: false,
                        auto_approve_prs: None,
                        lanes: None,
                        max_sessions: None,
                        diverse_review: None,
                        error: Some(missing_session()),
                    });
                }
                Err(e) => {
                    return Json(UserSettingsSetResponse {
                        ok: false,
                        applied: false,
                        auto_approve_prs: None,
                        lanes: None,
                        max_sessions: None,
                        diverse_review: None,
                        error: Some(e),
                    });
                }
            };
        let repo = UserSettingsRepository::new(self.state.db().clone());

        let err = |msg: String| {
            Json(UserSettingsSetResponse {
                ok: false,
                applied: false,
                auto_approve_prs: None,
                lanes: None,
                max_sessions: None,
                diverse_review: None,
                error: Some(msg),
            })
        };

        let mut applied = false;

        // Per-user model lanes: validate every id (union across lanes) against
        // THIS user's connected providers before persisting, so a user can't
        // select a model on a provider they haven't connected.
        if let Some(lanes_payload) = p.lanes.as_ref() {
            let lanes: ModelLanes = lanes_payload.clone().into();
            let all = lanes.all_models();
            if let Err(e) = self
                .state
                .validate_models_for_user(&all, Some(&user_id))
                .await
            {
                return err(e);
            }
            if let Err(e) = repo.upsert_lanes(&user_id, &lanes).await {
                return err(e.to_string());
            }
            applied = true;
        }

        // Per-user, per-model concurrency caps. No validation against connected
        // providers (caps for not-yet-connected models are harmless — they only
        // bind once a model is actually dispatched); non-positive values are
        // dropped on read.
        if let Some(max_sessions) = p.max_sessions.as_ref() {
            if let Err(e) = repo.upsert_max_sessions(&user_id, max_sessions).await {
                return err(e.to_string());
            }
            applied = true;
        }

        if let Some(target) = p.auto_approve_prs {
            if let Err(e) = repo.upsert_auto_approve_prs(&user_id, target).await {
                return err(e.to_string());
            }
            applied = true;
        }

        // Cross-model ("Thorough") review toggle. No validation needed — it is a
        // pure dispatch-time preference; a degenerate single-model selection
        // falls back to same-model review.
        if let Some(target) = p.diverse_review {
            if let Err(e) = repo.upsert_diverse_review(&user_id, target).await {
                return err(e.to_string());
            }
            applied = true;
        }

        // A changed model selection or cap can make more work dispatchable now,
        // so kick a dispatch pass. No-op for auto-approve-only patches.
        if p.lanes.is_some() || p.max_sessions.is_some() {
            self.state.apply_user_model_change().await;
        }

        match repo.get_or_default(&user_id).await {
            Ok(s) => Json(UserSettingsSetResponse {
                ok: true,
                applied,
                auto_approve_prs: Some(s.auto_approve_prs),
                lanes: Some(s.lanes.unwrap_or_default().into()),
                max_sessions: Some(s.max_sessions.unwrap_or_default()),
                diverse_review: Some(s.diverse_review),
                error: None,
            }),
            Err(e) => err(e.to_string()),
        }
    }
}
