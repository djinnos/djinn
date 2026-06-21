use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Per-user, per-ROLE ordered model selection ("lanes"). Each lane is an
/// ordered fallback list (highest priority first) of full `provider/model`
/// ids. A task's base role maps to exactly one lane:
///
/// - `plan`      → planner, architect, chat
/// - `implement` → worker
/// - `review`    → reviewer
///
/// (`lead` is a human role with no model; dispatch fallbacks treat it as
/// `plan`.) Persisted as a single JSON-object TEXT column
/// (`user_settings.model_lanes`, migration 77). An all-empty `ModelLanes`
/// reads back as `None` on `UserSettings.lanes` → global fallback.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLanes {
    #[serde(default)]
    pub plan: Vec<String>,
    #[serde(default)]
    pub implement: Vec<String>,
    #[serde(default)]
    pub review: Vec<String>,
}

/// The three lanes a per-user model selection is split across.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelLane {
    Plan,
    Implement,
    Review,
}

impl ModelLane {
    /// Map a dispatch base role to its lane. `lead` is a human role with no
    /// model; it falls back to the `plan` lane so escalation paths that resolve
    /// a lead model still have a candidate.
    pub fn for_role(base_role: &str) -> Self {
        match base_role {
            "worker" => ModelLane::Implement,
            "reviewer" => ModelLane::Review,
            // planner, architect, chat, lead, and anything unknown → plan
            _ => ModelLane::Plan,
        }
    }
}

impl ModelLanes {
    /// True when every lane is empty (→ treated as "no explicit selection").
    pub fn is_empty(&self) -> bool {
        self.plan.is_empty() && self.implement.is_empty() && self.review.is_empty()
    }

    /// The ordered list for a given lane.
    pub fn lane(&self, lane: ModelLane) -> &[String] {
        match lane {
            ModelLane::Plan => &self.plan,
            ModelLane::Implement => &self.implement,
            ModelLane::Review => &self.review,
        }
    }

    /// The ordered list for the lane a base role maps to.
    pub fn for_role(&self, base_role: &str) -> &[String] {
        self.lane(ModelLane::for_role(base_role))
    }

    /// Distinct union of all model ids across all three lanes (order: plan,
    /// implement, review; duplicates dropped). Used by the slot pool to size
    /// capacity for everything a user might dispatch.
    pub fn all_models(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for m in self.plan.iter().chain(&self.implement).chain(&self.review) {
            if seen.insert(m.clone()) {
                out.push(m.clone());
            }
        }
        out
    }

    /// Build lanes from a legacy flat list by seeding every lane with the same
    /// ordered selection. Used by the cut-over migration's read path and any
    /// remaining flat-list callers.
    pub fn from_flat(models: Vec<String>) -> Self {
        Self {
            plan: models.clone(),
            implement: models.clone(),
            review: models,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct UserSettings {
    pub user_id: String,
    pub auto_approve_prs: bool,
    /// Per-user, per-ROLE ordered model selection ("lanes"). `None` = no
    /// explicit selection → callers fall back to the global deployment model
    /// list. Persisted as a JSON-object TEXT column (`user_settings.model_lanes`,
    /// migration 77).
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub lanes: Option<ModelLanes>,
    /// Per-user, per-model concurrency caps (`{ "provider/model": cap }`). The
    /// sole admission control at dispatch — the slot pool spawns on demand, with
    /// no global ceiling. `None`/absent ⇒ default 1 per selected model.
    /// Persisted as a JSON-object TEXT column (`user_settings.max_sessions`,
    /// migration 32). Caps are per-model, shared across lanes.
    #[cfg_attr(feature = "sqlx", sqlx(default))]
    pub max_sessions: Option<HashMap<String, u32>>,
    /// Cross-model ("Thorough") review. When `true` (the default), a task
    /// dispatched to the reviewer role prefers a model id different from the one
    /// that implemented the task — the reviewer walks its review-lane fallback
    /// list and takes the first id that differs from the implementer's. A
    /// degenerate single-model selection silently falls back to same-model.
    /// Persisted as a NOT-NULL boolean column (`user_settings.diverse_review`,
    /// migration 78, default TRUE).
    pub diverse_review: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl UserSettings {
    /// All-defaults-off row for users who have never written a setting. Lets
    /// the read path return `Some(defaults)` without inserting a row, so a
    /// `get` call for a freshly-created user doesn't pollute the table.
    pub fn defaults_for(user_id: &str) -> Self {
        Self {
            user_id: user_id.to_string(),
            auto_approve_prs: false,
            lanes: None,
            max_sessions: None,
            // Cross-model review is on by default (DB column default TRUE), so a
            // never-written user gets the same behavior as a migrated row.
            diverse_review: true,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
}
