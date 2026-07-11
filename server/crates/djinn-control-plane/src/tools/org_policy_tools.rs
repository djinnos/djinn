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
//! - **Recommended-model overrides.** Admin-curated additions and demotions
//!   from the baseline `RECOMMENDED_MODELS` set, each a fully-qualified
//!   `provider/model-id`.
//!
//! Both the read and the write are admin-gated. Non-admins receive the
//! enforced/filtered results elsewhere but can neither see nor edit the policy.

use std::collections::HashSet;

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
    /// Fully-qualified `provider/model-id` entries to add to the recommended set.
    pub additional_recommended_model_ids: Vec<String>,
    /// Fully-qualified `provider/model-id` entries to demote from the recommended set.
    pub demoted_recommended_model_ids: Vec<String>,
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
    /// Fully-qualified `provider/model-id` entries to add to the recommended
    /// set on top of the baseline. Omit to keep the current value; pass an
    /// empty list to clear.
    #[serde(default)]
    pub additional_recommended_model_ids: Option<Vec<String>>,
    /// Fully-qualified `provider/model-id` entries to demote from the
    /// recommended set. Omit to keep the current value; pass an empty list to
    /// clear.
    #[serde(default)]
    pub demoted_recommended_model_ids: Option<Vec<String>>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct OrgPolicySetResponse {
    pub ok: bool,
    pub applied: bool,
    pub blocked_subscriptions: Vec<String>,
    pub default_lanes: OrgDefaultLanesPayload,
    pub lock_level: String,
    pub additional_recommended_model_ids: Vec<String>,
    pub demoted_recommended_model_ids: Vec<String>,
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

/// Canonical key used to collapse duplicate provider ids that name the same
/// real subscription (e.g. the catalog carries both `github-copilot` and
/// `githubcopilot` — same GitHub Copilot sub). Lowercased, separators stripped.
fn canonical_sub_key(id: &str) -> String {
    id.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

/// Build the set of all known provider ids from the builtin registry plus the
/// live catalog (so dynamically-discovered models.dev providers are accepted).
/// This is used to validate recommended-model override entries: known providers
/// are accepted even when not currently connected, enabling admins to pre-stage
/// policy.
fn known_provider_ids(server: &DjinnMcpServer) -> HashSet<String> {
    let mut ids: HashSet<String> = builtin::builtin_provider_ids();
    for p in server.state.catalog().list_providers() {
        ids.insert(p.id);
    }
    ids
}

/// Validate and canonicalize a list of recommended-model override ids.
///
/// Each entry must be a fully-qualified `provider/model-id` where:
/// - The provider prefix is non-empty and known to the deployment.
/// - The model id (everything after the first `/`) is non-empty.
/// - Model ids may contain additional `/` path segments (e.g.
///   `fireworks-ai/accounts/fireworks/models/glm-5p2`).
///
/// Rejects:
/// - Empty or whitespace-only entries.
/// - Raw local ids (no `/` separator).
/// - Malformed qualified ids (empty provider or empty model id).
/// - Provider ids unknown to the deployment.
/// - Duplicate ids within the list.
///
/// On success returns the deduplicated, sorted list.
fn validate_model_override_list(
    raw_ids: &[String],
    known: &HashSet<String>,
    list_label: &str,
) -> Result<Vec<String>, String> {
    let mut validated: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for raw in raw_ids {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(format!(
                "{list_label}: model id `{raw}` is malformed — empty or whitespace-only entries are not allowed"
            ));
        }

        // Must have at least one `/` separating provider from model id.
        let (provider, model_rest) = match trimmed.split_once('/') {
            Some((p, m)) => (p, m),
            None => {
                return Err(format!(
                    "{list_label}: model id `{trimmed}` is not fully qualified — \
                     expected `provider/model-id` but found no `/` separator"
                ));
            }
        };

        if provider.is_empty() {
            return Err(format!(
                "{list_label}: model id `{trimmed}` has an empty provider prefix"
            ));
        }
        if model_rest.is_empty() {
            return Err(format!(
                "{list_label}: model id `{trimmed}` has an empty model id after the provider prefix"
            ));
        }

        // Provider must be known to the deployment (builtin or catalog).
        if !known.contains(provider) {
            return Err(format!(
                "{list_label}: provider `{provider}` in model id `{trimmed}` \
                 is not a known provider in this deployment"
            ));
        }

        // Check for duplicates (case-sensitive — model ids are canonical).
        if !seen.insert(trimmed.to_string()) {
            return Err(format!("{list_label}: duplicate model id `{trimmed}`"));
        }

        validated.push(trimmed.to_string());
    }

    validated.sort();
    Ok(validated)
}

/// Detect model ids present in both the addition and demotion lists.
fn detect_cross_list_overlap(additional: &[String], demoted: &[String]) -> Option<String> {
    let demoted_set: HashSet<&String> = demoted.iter().collect();
    for id in additional {
        if demoted_set.contains(id) {
            return Some(id.clone());
        }
    }
    None
}

/// The de-duplicated universe of **governable subscriptions** for the admin
/// allow/block table: each supported subscription provider once, paired with a
/// display name. Drawn from the live catalog (so newly-surfaced models.dev
/// subscriptions appear automatically), plus the merged-child subscriptions the
/// catalog hides (notably ChatGPT/Codex, which merges into `openai`). Duplicate
/// ids for the same real sub (e.g. the two GitHub Copilot entries) collapse to a
/// single row. Non-subscription API providers are never included.
///
/// Each entry is `(id, display_name)` where `id` is the stored/blocklist form.
fn subscription_universe(server: &DjinnMcpServer) -> Vec<(String, String)> {
    let merged = builtin::merged_provider_ids();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<(String, String)> = Vec::new();

    // 1. Merged-child subscriptions the catalog hides (e.g. chatgpt_codex →
    //    openai). These are governable in their own right even though their
    //    models surface under the parent namespace, so they must appear here.
    for bp in builtin::BUILTIN_PROVIDERS {
        if bp.merge_into.is_some() && builtin::is_subscription_provider(bp.id) {
            let key = canonical_sub_key(bp.id);
            if seen.insert(key) {
                out.push((bp.id.to_string(), bp.display_name.to_string()));
            }
        }
    }

    // 2. Subscriptions the catalog exposes directly (skip hidden merged parents'
    //    children already added above, and collapse alias duplicates).
    for p in server.state.catalog().list_providers() {
        if merged.contains(&p.id) || !builtin::is_subscription_provider(&p.id) {
            continue;
        }
        let key = canonical_sub_key(&p.id);
        if seen.insert(key) {
            out.push((p.id.clone(), p.name.clone()));
        }
    }

    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Build a default/empty error response for `org_policy_set`, populating every
/// field with safe defaults.
fn default_set_error_response(msg: String) -> OrgPolicySetResponse {
    OrgPolicySetResponse {
        ok: false,
        applied: false,
        blocked_subscriptions: vec![],
        default_lanes: OrgDefaultLanesPayload::default(),
        lock_level: LockLevel::Flexible.as_db().to_string(),
        additional_recommended_model_ids: vec![],
        demoted_recommended_model_ids: vec![],
        error: Some(msg),
    }
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
        additional_recommended_model_ids: policy.additional_recommended_model_ids.clone(),
        demoted_recommended_model_ids: policy.demoted_recommended_model_ids.clone(),
        error: None,
    }
}

#[tool_router(router = org_policy_tool_router, vis = "pub")]
impl DjinnMcpServer {
    #[tool(
        description = "Admin-only: read the org AI policy — the subscription \
        allow/block table (with data-residency jurisdiction per provider), the \
        org-default per-role model lanes, the lane lock level, and the \
        recommended-model override lists (additional/demoted).",
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
                additional_recommended_model_ids: vec![],
                demoted_recommended_model_ids: vec![],
                error: Some(error),
            });
        }
        let policy = self.state.org_ai_policy().await;
        Json(build_get_response(self, &policy))
    }

    #[tool(
        description = "Admin-only: update the org AI policy. Patch the blocked \
        subscription set (only subscription providers are honored; admin API \
        keys are never blocked), the org-default per-role lanes, the lane lock \
        level, and/or the recommended-model override lists (additional/demoted). \
        Omitted fields keep their current value."
    )]
    pub async fn org_policy_set(
        &self,
        Parameters(p): Parameters<OrgPolicySetParams>,
    ) -> Json<OrgPolicySetResponse> {
        let err = |msg: String| Json(default_set_error_response(msg));

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

        // ── Recommended-model override lists ──────────────────────────────
        let known = known_provider_ids(self);

        let mut additional_changed = false;
        if let Some(raw_additional) = p.additional_recommended_model_ids {
            match validate_model_override_list(
                &raw_additional,
                &known,
                "additional_recommended_model_ids",
            ) {
                Ok(validated) => {
                    policy.additional_recommended_model_ids = validated;
                    additional_changed = true;
                }
                Err(e) => return err(e),
            }
        }

        if let Some(raw_demoted) = p.demoted_recommended_model_ids {
            match validate_model_override_list(
                &raw_demoted,
                &known,
                "demoted_recommended_model_ids",
            ) {
                Ok(validated) => {
                    policy.demoted_recommended_model_ids = validated;
                    applied = true;
                }
                Err(e) => return err(e),
            }
        }
        if additional_changed {
            applied = true;
        }

        // Cross-list overlap: reject if the same id appears in both lists.
        if let Some(overlap) = detect_cross_list_overlap(
            &policy.additional_recommended_model_ids,
            &policy.demoted_recommended_model_ids,
        ) {
            return err(format!(
                "model id `{overlap}` appears in both \
                 additional_recommended_model_ids and \
                 demoted_recommended_model_ids — an id cannot be both \
                 added and demoted"
            ));
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
            additional_recommended_model_ids: saved.additional_recommended_model_ids,
            demoted_recommended_model_ids: saved.demoted_recommended_model_ids,
            error: None,
        })
    }
}
