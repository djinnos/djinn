use serde::{Deserialize, Serialize};

/// Org-default lane assignment lock level.
///
/// - `Flexible`: the org default seeds a member's lanes when they have none,
///   but the member may freely override within the allowed set.
/// - `Locked`: the org default is authoritative — member lane edits are
///   prevented (UI disabled + server-side guard rejects writes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LockLevel {
    /// Members may override the org default within the allowed set.
    #[default]
    Flexible,
    /// Org assignment is authoritative; member lane edits are blocked.
    Locked,
}

impl LockLevel {
    /// Parse from the DB string column. Unknown values degrade to `Flexible`
    /// (the safe, non-restrictive default).
    pub fn from_db(raw: &str) -> Self {
        match raw {
            "locked" => LockLevel::Locked,
            _ => LockLevel::Flexible,
        }
    }

    /// The DB string representation.
    pub fn as_db(self) -> &'static str {
        match self {
            LockLevel::Flexible => "flexible",
            LockLevel::Locked => "locked",
        }
    }

    /// True when member lane edits are disallowed.
    pub fn is_locked(self) -> bool {
        matches!(self, LockLevel::Locked)
    }
}

/// Org-default per-ROLE lane assignment that new members inherit when they have
/// no explicit selection of their own. Mirrors the per-user `ModelLanes` shape
/// (ordered `provider/model` ids per lane, highest priority first) so the org
/// default can be applied to a member directly.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgDefaultLanes {
    #[serde(default)]
    pub plan: Vec<String>,
    #[serde(default)]
    pub implement: Vec<String>,
    #[serde(default)]
    pub review: Vec<String>,
}

impl OrgDefaultLanes {
    /// True when every lane is empty (→ no org default assignment).
    pub fn is_empty(&self) -> bool {
        self.plan.is_empty() && self.implement.is_empty() && self.review.is_empty()
    }
}

/// Org-wide AI policy (admin-owned, singleton). Governs:
///
/// - **Subscription allow/block** (`blocked_subscriptions`): the set of
///   subscription provider ids that are filtered out of every member-facing
///   provider/model list and rejected by per-user model validation. Admin API
///   keys (non-subscription providers) are NOT governed by this list.
/// - **Org default lanes + lock** (`default_lanes`, `lock_level`): the
///   org-default per-role model assignment new members inherit, and whether
///   members may override it.
/// - **Recommended-model overrides** (`additional_recommended_model_ids`,
///   `demoted_recommended_model_ids`): admin-curated additions and removals
///   from the baseline `RECOMMENDED_MODELS` set. Each entry is a fully
///   qualified `provider/model-id`. Demotion wins over addition at runtime.
///
/// Persisted as a single singleton row (`org_ai_policy`, migration 79).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgAiPolicy {
    /// Subscription provider ids that are blocked org-wide. Hidden from member
    /// catalogs and rejected by model validation. Only subscription providers
    /// belong here; non-subscription (API-key) providers are never blocked.
    #[serde(default)]
    pub blocked_subscriptions: Vec<String>,
    /// Org-default per-role lanes new members inherit when they have none.
    #[serde(default)]
    pub default_lanes: OrgDefaultLanes,
    /// Whether the org default is authoritative (`Locked`) or merely a seed
    /// members may override (`Flexible`).
    #[serde(default)]
    pub lock_level: LockLevel,
    /// Fully-qualified `provider/model-id` entries the admin wants marked
    /// recommended on top of the baseline `RECOMMENDED_MODELS` set. Stored
    /// deduplicated and sorted.
    #[serde(default)]
    pub additional_recommended_model_ids: Vec<String>,
    /// Fully-qualified `provider/model-id` entries the admin wants removed
    /// from the recommended set (including baseline entries). Demotion wins
    /// over addition. Stored deduplicated and sorted.
    #[serde(default)]
    pub demoted_recommended_model_ids: Vec<String>,
    pub updated_at: String,
}

impl OrgAiPolicy {
    /// True when `provider_id` is blocked org-wide (case-insensitive).
    pub fn is_blocked(&self, provider_id: &str) -> bool {
        let needle = provider_id.to_ascii_lowercase();
        self.blocked_subscriptions
            .iter()
            .any(|b| b.eq_ignore_ascii_case(&needle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_level_round_trips_via_db_string() {
        assert_eq!(LockLevel::from_db("locked"), LockLevel::Locked);
        assert_eq!(LockLevel::from_db("flexible"), LockLevel::Flexible);
        // Unknown degrades to the non-restrictive default.
        assert_eq!(LockLevel::from_db("bogus"), LockLevel::Flexible);
        assert_eq!(LockLevel::Locked.as_db(), "locked");
        assert_eq!(LockLevel::Flexible.as_db(), "flexible");
        assert!(LockLevel::Locked.is_locked());
        assert!(!LockLevel::Flexible.is_locked());
    }

    #[test]
    fn is_blocked_is_case_insensitive() {
        let policy = OrgAiPolicy {
            blocked_subscriptions: vec!["MiniMax-Coding-Plan".to_string()],
            ..Default::default()
        };
        assert!(policy.is_blocked("minimax-coding-plan"));
        assert!(policy.is_blocked("MINIMAX-CODING-PLAN"));
        assert!(!policy.is_blocked("anthropic"));
    }
}
