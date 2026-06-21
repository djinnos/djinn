//! Admin-only org AI policy exposed over MCP.
//!
//! Backs the admin "AI Policy" surface (slice 5 of proposal p8py):
//!
//! - **Subscription allow/block, framed by data residency.** Admins block
//!   individual subscription providers (or, via the UI, all China-hosted ones).
//!   Blocked subscriptions are filtered out of every member-facing provider/
//!   model list and rejected by per-user model validation. Admin API keys
//!   (non-subscription providers) are NOT governed here.
//! - **Org default lanes + lock level.** An org-default per-role lane
//!   assignment new members inherit when they have none, plus whether members
//!   may override it (`flexible`) or not (`locked`).
//!
//! Both the read and the write are admin-gated. Non-admins receive the
//! enforced/filtered results elsewhere but can neither see nor edit the policy.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::require_admin;
use djinn_core::models::{LockLevel, OrgAiPolicy, OrgDefaultLanes};
use djinn_db::OrgAiPolicyRepository;
use djinn_provider::catalog::builtin::{self, Jurisdiction};

/// Org-default per-role lanes over the wire (mirrors the per-user lane shape).
#[derive(Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OrgDefaultLanesPayload {
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

impl From<OrgDefaultLanes> for OrgDefaultLanesPayload {
    fn from(l: OrgDefaultLanes) -> Self {
        Self {
            plan: l.plan,
            implement: l.implement,
            review: l.review,
        }
    }
}

impl From<OrgDefaultLanesPayload> for OrgDefaultLanes {
    fn from(p: OrgDefaultLanesPayload) -> Self {
        Self {
            plan: p.plan,
            implement: p.implement,
            review: p.review,
        }
    }
}

/// One subscription provider as the admin policy surface sees it: its id,
/// display name, data-residency jurisdiction, and whether it is currently
/// blocked. Lets the UI render the allow/block table grouped by jurisdiction
/// without re-deriving the residency map client-side.
#[derive(Clone, Debug, Serialize, schemars::JsonSchema)]
pub struct SubscriptionPolicyItem {
    pub id: String,
    pub name: String,
    /// Data residency: `us`, `eu`, `cn`, or `other` (djinn-owned classification).
    pub jurisdiction: String,
    /// Whether this subscription is currently blocked org-wide.
    pub blocked: bool,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct OrgPolicyGetParams {}

#[derive(Serialize, schemars::JsonSchema)]
pub struct OrgPolicyGetResponse {
    pub ok: bool,
    /// Every known subscription provider with its jurisdiction + blocked flag.
    pub subscriptions: Vec<SubscriptionPolicyItem>,
    /// The blocked subscription provider ids (subset of `subscriptions`).
    pub blocked_subscriptions: Vec<String>,
    /// Org-default per-role lanes new members inherit when they have none.
    pub default_lanes: OrgDefaultLanesPayload,
    /// `flexible` (members may override) | `locked` (org assignment authoritative).
    pub lock_level: String,
    pub error: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OrgPolicySetParams {
    /// Replacement set of blocked subscription provider ids. Only subscription
    /// providers are honored; any non-subscription id is ignored. Omit to keep
    /// the current value.
    #[serde(default)]
    pub blocked_subscriptions: Option<Vec<String>>,
    /// Org-default per-role lanes new members inherit. Pass all-empty to clear.
    /// Omit to keep the current value.
    #[serde(default)]
    pub default_lanes: Option<OrgDefaultLanesPayload>,
    /// Lane lock level: `flexible` or `locked`. Omit to keep the current value.
    #[serde(default)]
    pub lock_level: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct OrgPolicySetResponse {
    pub ok: bool,
    pub applied: bool,
    pub blocked_subscriptions: Vec<String>,
    pub default_lanes: OrgDefaultLanesPayload,
    pub lock_level: String,
    pub error: Option<String>,
}

fn jurisdiction_str(j: Jurisdiction) -> &'static str {
    match j {
        Jurisdiction::Us => "us",
        Jurisdiction::Eu => "eu",
        Jurisdiction::Cn => "cn",
        Jurisdiction::Other => "other",
    }
}

/// All subscription provider ids the catalog currently exposes, paired with
/// their display name. This is the universe the admin allow/block table is
/// drawn over — drawn from the live catalog so newly-surfaced models.dev
/// subscriptions appear automatically.
fn subscription_universe(server: &DjinnMcpServer) -> Vec<(String, String)> {
    let merged = builtin::merged_provider_ids();
    let mut out: Vec<(String, String)> = server
        .state
        .catalog()
        .list_providers()
        .into_iter()
        .filter(|p| !merged.contains(&p.id))
        .filter(|p| builtin::is_subscription_provider(&p.id))
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

fn build_get_response(server: &DjinnMcpServer, policy: &OrgAiPolicy) -> OrgPolicyGetResponse {
    let subscriptions = subscription_universe(server)
        .into_iter()
        .map(|(id, name)| {
            let jurisdiction = jurisdiction_str(builtin::provider_jurisdiction(&id)).to_string();
            let blocked = policy.is_blocked(&id);
            SubscriptionPolicyItem {
                id,
                name,
                jurisdiction,
                blocked,
            }
        })
        .collect();
    OrgPolicyGetResponse {
        ok: true,
        subscriptions,
        blocked_subscriptions: policy.blocked_subscriptions.clone(),
        default_lanes: policy.default_lanes.clone().into(),
        lock_level: policy.lock_level.as_db().to_string(),
        error: None,
    }
}

#[tool_router(router = org_policy_tool_router, vis = "pub")]
impl DjinnMcpServer {
    #[tool(
        description = "Admin-only: read the org AI policy — the subscription \
        allow/block table (with data-residency jurisdiction per provider), the \
        org-default per-role model lanes, and the lane lock level.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub async fn org_policy_get(
        &self,
        Parameters(_p): Parameters<OrgPolicyGetParams>,
    ) -> Json<OrgPolicyGetResponse> {
        if let Err(error) = require_admin(self.state.db()).await {
            return Json(OrgPolicyGetResponse {
                ok: false,
                subscriptions: vec![],
                blocked_subscriptions: vec![],
                default_lanes: OrgDefaultLanesPayload::default(),
                lock_level: LockLevel::Flexible.as_db().to_string(),
                error: Some(error),
            });
        }
        let policy = self.state.org_ai_policy().await;
        Json(build_get_response(self, &policy))
    }

    #[tool(
        description = "Admin-only: update the org AI policy. Patch the blocked \
        subscription set (only subscription providers are honored; admin API \
        keys are never blocked), the org-default per-role lanes, and/or the \
        lane lock level. Omitted fields keep their current value."
    )]
    pub async fn org_policy_set(
        &self,
        Parameters(p): Parameters<OrgPolicySetParams>,
    ) -> Json<OrgPolicySetResponse> {
        let err = |msg: String| {
            Json(OrgPolicySetResponse {
                ok: false,
                applied: false,
                blocked_subscriptions: vec![],
                default_lanes: OrgDefaultLanesPayload::default(),
                lock_level: LockLevel::Flexible.as_db().to_string(),
                error: Some(msg),
            })
        };

        if let Err(error) = require_admin(self.state.db()).await {
            return err(error);
        }

        let repo = OrgAiPolicyRepository::new(self.state.db().clone());
        let mut policy = match repo.get().await {
            Ok(p) => p,
            Err(e) => return err(e.to_string()),
        };

        let mut applied = false;

        if let Some(blocked) = p.blocked_subscriptions {
            // Only subscription providers may be blocked; silently drop any
            // non-subscription id (admin API keys are never governed here).
            // Normalize to lowercase + dedupe for a stable stored set.
            let mut cleaned: Vec<String> = blocked
                .into_iter()
                .map(|id| id.trim().to_ascii_lowercase())
                .filter(|id| !id.is_empty() && builtin::is_subscription_provider(id))
                .collect();
            cleaned.sort();
            cleaned.dedup();
            policy.blocked_subscriptions = cleaned;
            applied = true;
        }

        if let Some(lanes) = p.default_lanes {
            policy.default_lanes = lanes.into();
            applied = true;
        }

        if let Some(lock) = p.lock_level {
            let normalized = lock.trim().to_ascii_lowercase();
            if normalized != "flexible" && normalized != "locked" {
                return err(format!(
                    "lock_level must be `flexible` or `locked`, got `{lock}`"
                ));
            }
            policy.lock_level = LockLevel::from_db(&normalized);
            applied = true;
        }

        let saved = match repo.set(&policy).await {
            Ok(s) => s,
            Err(e) => return err(e.to_string()),
        };

        // A changed blocklist or org default can shift what's dispatchable for
        // members (e.g. a blocked sub must stop sizing the slot pool), so kick a
        // dispatch/capacity recompute.
        self.state.apply_user_model_change().await;

        Json(OrgPolicySetResponse {
            ok: true,
            applied,
            blocked_subscriptions: saved.blocked_subscriptions,
            default_lanes: saved.default_lanes.into(),
            lock_level: saved.lock_level.as_db().to_string(),
            error: None,
        })
    }
}
