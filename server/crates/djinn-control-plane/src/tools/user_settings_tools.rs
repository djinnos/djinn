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
use djinn_core::models::{LaneMaxSessions, ModelLanes, OrgAiPolicy};
use djinn_db::UserSettingsRepository;

/// The org-default lanes as a wire payload (all-empty when unset → global).
fn org_default_lanes_payload(policy: &OrgAiPolicy) -> ModelLanesPayload {
    ModelLanesPayload {
        plan: policy.default_lanes.plan.clone(),
        implement: policy.default_lanes.implement.clone(),
        review: policy.default_lanes.review.clone(),
    }
}

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

/// Per-user concurrency ceilings for each role lane.
#[derive(Clone, Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LaneMaxSessionsPayload {
    /// Concurrent autonomous planning and refinement sessions. Interactive
    /// chat is not subject to this ceiling.
    #[schemars(with = "i64", range(min = 1, max = 10))]
    pub plan: u32,
    /// Concurrent worker sessions.
    #[schemars(with = "i64", range(min = 1, max = 10))]
    pub implement: u32,
    /// Concurrent reviewer sessions.
    #[schemars(with = "i64", range(min = 1, max = 10))]
    pub review: u32,
}

impl From<LaneMaxSessions> for LaneMaxSessionsPayload {
    fn from(limits: LaneMaxSessions) -> Self {
        Self {
            plan: limits.plan,
            implement: limits.implement,
            review: limits.review,
        }
    }
}

impl From<LaneMaxSessionsPayload> for LaneMaxSessions {
    fn from(payload: LaneMaxSessionsPayload) -> Self {
        Self {
            plan: payload.plan,
            implement: payload.implement,
            review: payload.review,
        }
    }
}

fn validate_lane_max_sessions(payload: &LaneMaxSessionsPayload) -> Result<(), String> {
    for (lane, value) in [
        ("plan", payload.plan),
        ("implement", payload.implement),
        ("review", payload.review),
    ] {
        if !(LaneMaxSessions::MIN..=LaneMaxSessions::MAX).contains(&value) {
            return Err(format!(
                "lane_max_sessions.{lane} must be between {} and {}",
                LaneMaxSessions::MIN,
                LaneMaxSessions::MAX
            ));
        }
    }
    Ok(())
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
    /// When the org lane policy is `locked`, this echoes the org default
    /// (member edits are ignored).
    pub lanes: ModelLanesPayload,
    /// True when the org AI policy locks lane assignment: the member may not
    /// edit lanes (UI disables the controls; the server rejects lane writes).
    pub lane_locked: bool,
    /// This user's per-model concurrency caps (`{ "provider/model": cap }`).
    /// Per-model admission ceilings at dispatch, composed with any per-lane
    /// ceiling; empty ⇒ default 1 per model.
    #[schemars(with = "std::collections::HashMap<String, i64>")]
    pub max_sessions: HashMap<String, u32>,
    /// Per-lane concurrency ceilings. `None` means this user has no
    /// lane-specific ceiling (legacy/unbounded behavior).
    pub lane_max_sessions: Option<LaneMaxSessionsPayload>,
    /// Cross-model ("Thorough") review. When true (the default), a task
    /// dispatched to the reviewer role prefers a model id different from the one
    /// that implemented it. A degenerate single-model selection falls back to
    /// same-model review.
    pub diverse_review: bool,
    /// Cross-model ("Diverse") refinement. When true (the default), the
    /// proposal-refinement roles (advocate, adversary, judge) prefer a model id
    /// different from the primary task model. Falls back to same-model when
    /// alternatives are unavailable.
    pub diverse_refinement: bool,
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
    /// per-model admission ceiling (there is no global ceiling), composed with
    /// any per-lane ceiling. Pass `{}` to clear (→ default 1 per model). Omit to
    /// keep the current value.
    #[schemars(with = "Option<std::collections::HashMap<String, i64>>")]
    pub max_sessions: Option<HashMap<String, u32>>,
    /// Per-lane concurrency ceilings for THIS user. Every value must be in
    /// 1..=10. Omit to keep the current value.
    pub lane_max_sessions: Option<LaneMaxSessionsPayload>,
    /// Enable or disable cross-model ("Thorough") review for THIS user. When on
    /// (the default), the reviewer prefers a model id different from the
    /// implementer's. Omit to keep the current value.
    pub diverse_review: Option<bool>,
    /// Enable or disable cross-model ("Diverse") refinement for THIS user. When
    /// on (the default), proposal-refinement roles prefer a different model from
    /// the primary task model. Falls back to same-model when alternatives are
    /// unavailable. Omit to keep the current value.
    pub diverse_refinement: Option<bool>,
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
    /// The resulting per-lane concurrency ceilings after the patch. `None`
    /// means no lane-specific ceiling.
    pub lane_max_sessions: Option<LaneMaxSessionsPayload>,
    /// The resulting cross-model review toggle after the patch.
    pub diverse_review: Option<bool>,
    /// The resulting cross-model refinement toggle after the patch.
    pub diverse_refinement: Option<bool>,
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
                        lane_locked: false,
                        max_sessions: HashMap::new(),
                        lane_max_sessions: None,
                        diverse_review: true,
                        diverse_refinement: true,
                        error: Some(missing_session()),
                    });
                }
                Err(e) => {
                    return Json(UserSettingsGetResponse {
                        ok: false,
                        user_id: None,
                        auto_approve_prs: false,
                        lanes: ModelLanesPayload::default(),
                        lane_locked: false,
                        max_sessions: HashMap::new(),
                        lane_max_sessions: None,
                        diverse_review: true,
                        diverse_refinement: true,
                        error: Some(e),
                    });
                }
            };
        let repo = UserSettingsRepository::new(self.state.db().clone());
        match repo.get_or_default(&user_id).await {
            Ok(s) => {
                // Org-default lane inheritance + lock: a member with no explicit
                // lanes inherits the org default; under a `locked` policy the
                // org default is authoritative regardless of any member value.
                let policy = self.state.org_ai_policy().await;
                let lane_locked = policy.lock_level.is_locked();
                let lanes: ModelLanesPayload = match s.lanes {
                    Some(l) if !lane_locked => l.into(),
                    // No member lanes, or locked → org default (which may be
                    // all-empty = global fallback).
                    _ => org_default_lanes_payload(&policy),
                };
                Json(UserSettingsGetResponse {
                    ok: true,
                    user_id: Some(user_id),
                    auto_approve_prs: s.auto_approve_prs,
                    lanes,
                    lane_locked,
                    max_sessions: s.max_sessions.unwrap_or_default(),
                    lane_max_sessions: s.lane_max_sessions.map(Into::into),
                    diverse_review: s.diverse_review,
                    diverse_refinement: s.diverse_refinement,
                    error: None,
                })
            }
            Err(e) => Json(UserSettingsGetResponse {
                ok: false,
                user_id: Some(user_id),
                auto_approve_prs: false,
                lanes: ModelLanesPayload::default(),
                lane_locked: false,
                max_sessions: HashMap::new(),
                lane_max_sessions: None,
                diverse_review: true,
                diverse_refinement: true,
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
                        lane_max_sessions: None,
                        diverse_review: None,
                        diverse_refinement: None,
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
                        lane_max_sessions: None,
                        diverse_review: None,
                        diverse_refinement: None,
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
                lane_max_sessions: None,
                diverse_review: None,
                diverse_refinement: None,
                error: Some(msg),
            })
        };

        let mut applied = false;

        // Validate all supplied limits before performing any patch writes, so
        // an invalid lane limit cannot leave earlier fields partially applied.
        if let Some(limits) = p.lane_max_sessions.as_ref()
            && let Err(e) = validate_lane_max_sessions(limits)
        {
            return err(e);
        }

        // Per-user model lanes: validate every id (union across lanes) against
        // THIS user's connected providers before persisting, so a user can't
        // select a model on a provider they haven't connected.
        if let Some(lanes_payload) = p.lanes.as_ref() {
            // Org lane lock: under a `locked` policy the org default is
            // authoritative and members may not edit their lanes. Reject the
            // write rather than silently dropping it so the UI can surface why.
            if self.state.org_ai_policy().await.lock_level.is_locked() {
                return err("lane assignment is locked by your org's AI policy; \
                     ask an admin to change it"
                    .to_string());
            }
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

        if let Some(limits_payload) = p.lane_max_sessions.as_ref() {
            let limits: LaneMaxSessions = limits_payload.clone().into();
            if let Err(e) = repo.upsert_lane_max_sessions(&user_id, &limits).await {
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

        // Cross-model ("Diverse") refinement toggle for proposal-refinement
        // roles (advocate, adversary, judge). No validation needed — same
        // best-effort model-divergence strategy; a degenerate single-model
        // selection falls back to same-model refinement.
        if let Some(target) = p.diverse_refinement {
            if let Err(e) = repo.upsert_diverse_refinement(&user_id, target).await {
                return err(e.to_string());
            }
            applied = true;
        }

        // A changed model selection or cap can make more work dispatchable now,
        // so kick a dispatch pass. No-op for auto-approve-only patches.
        if p.lanes.is_some() || p.max_sessions.is_some() || p.lane_max_sessions.is_some() {
            self.state.apply_user_model_change().await;
        }

        match repo.get_or_default(&user_id).await {
            Ok(s) => Json(UserSettingsSetResponse {
                ok: true,
                applied,
                auto_approve_prs: Some(s.auto_approve_prs),
                lanes: Some(s.lanes.unwrap_or_default().into()),
                max_sessions: Some(s.max_sessions.unwrap_or_default()),
                lane_max_sessions: s.lane_max_sessions.map(Into::into),
                diverse_review: Some(s.diverse_review),
                diverse_refinement: Some(s.diverse_refinement),
                error: None,
            }),
            Err(e) => err(e.to_string()),
        }
    }
}
