use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Metadata recorded when dispatch is paused for a scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DispatchPause {
    pub paused_by: String,
    pub paused_at: String,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// Dispatch pause scope supported by the persistence layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DispatchPauseScope {
    Global,
    Project,
    User,
}

impl DispatchPauseScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

impl std::fmt::Display for DispatchPauseScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DispatchPauseScope {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw {
            "global" => Ok(Self::Global),
            "project" => Ok(Self::Project),
            "user" => Ok(Self::User),
            other => Err(format!("unknown dispatch pause scope `{other}`")),
        }
    }
}

/// Snapshot of dispatch pause state across all scopes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DispatchPauseState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub global: Option<DispatchPause>,
    #[serde(default)]
    pub projects: HashMap<String, DispatchPause>,
    #[serde(default)]
    pub users: HashMap<String, DispatchPause>,
}

/// A key-value setting persisted in the `settings` table.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Setting {
    pub key: String,
    pub value: String,
    pub updated_at: String,
}

/// Typed settings schema. Unknown fields are rejected at parse time.
#[derive(Clone, Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DjinnSettings {
    /// Maximum number of tasks to dispatch per cycle (default 50).
    #[schemars(with = "Option<i64>")]
    pub dispatch_limit: Option<u32>,
    /// Ordered list of models available to agents, e.g. `["openai/gpt-4o"]`.
    pub models: Option<Vec<String>>,
    /// LEGACY/ignored. Per-model concurrency caps are now **per-user**
    /// (`user_settings.max_sessions`) and the slot pool is elastic, so this
    /// global field is no longer written or read. Retained only so existing
    /// `settings.raw` rows still parse under `deny_unknown_fields`.
    #[schemars(with = "Option<HashMap<String, i64>>")]
    pub max_sessions: Option<HashMap<String, u32>>,
    /// Global emergency stop for task dispatch. Missing in older settings rows means unpaused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_pause: Option<DispatchPause>,
}

impl DjinnSettings {
    /// Deserialize from a raw DB value string, tolerating old/invalid formats
    /// by falling back to defaults with a warning.
    pub fn from_db_value(raw: &str) -> Self {
        // Fields removed in prior cut-overs are stripped before parsing so
        // DBs written under the old schema don't fall through to the lossy
        // legacy migrator just because of a now-unknown key.
        let cleaned = Self::strip_removed_fields(raw);
        let input = cleaned.as_deref().unwrap_or(raw);
        match serde_json::from_str::<Self>(input) {
            Ok(settings) => settings,
            Err(e) => {
                // Try parsing as legacy format and migrate what we can.
                if let Ok(legacy) = serde_json::from_str::<serde_json::Value>(raw) {
                    tracing::warn!(
                        error = %e,
                        "settings.raw uses legacy format — migrating to typed schema"
                    );
                    Self::from_legacy(&legacy)
                } else {
                    tracing::warn!(
                        error = %e,
                        "settings.raw is not valid JSON — using defaults"
                    );
                    Self::default()
                }
            }
        }
    }

    /// Remove fields that have been cut from the schema in prior migrations,
    /// returning the cleaned JSON if any stripping occurred. Keeps old DB
    /// rows parseable under `deny_unknown_fields` without falling through to
    /// the lossy legacy migrator.
    fn strip_removed_fields(raw: &str) -> Option<String> {
        const REMOVED_KEYS: &[&str] = &[
            "langfuse_public_key",
            "langfuse_secret_key",
            "langfuse_endpoint",
        ];
        let mut value: serde_json::Value = serde_json::from_str(raw).ok()?;
        let obj = value.as_object_mut()?;
        let mut stripped = false;
        for key in REMOVED_KEYS {
            if obj.remove(*key).is_some() {
                stripped = true;
            }
        }
        if stripped {
            serde_json::to_string(&value).ok()
        } else {
            None
        }
    }

    /// Best-effort migration from the old untyped JSON format.
    fn from_legacy(v: &serde_json::Value) -> Self {
        let dispatch_limit = v
            .get("coordinator")
            .and_then(|c| c.get("dispatch_limit"))
            .or_else(|| v.get("execution").and_then(|e| e.get("dispatch_limit")))
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as u32);

        let models = Self::extract_models_from_legacy(v);
        let max_sessions = Self::extract_max_sessions(v);

        Self {
            dispatch_limit,
            models,
            max_sessions,
            dispatch_pause: None,
        }
    }

    /// Extract a flat deduplicated model list from a legacy settings value.
    ///
    /// Handles several historical formats:
    /// - Very old untyped format: nested `coordinator.model_priority` or `execution.model_priority`
    ///   where `model_priority` is a `{role: [model_id, ...]}` map.
    /// - Intermediate typed format: flat `model_priority` at root (also a `{role: [model_id, ...]}` map).
    /// - Another legacy variant: `models.priority` nested map.
    fn extract_models_from_legacy(v: &serde_json::Value) -> Option<Vec<String>> {
        // Check if this is the intermediate typed format: flat `model_priority` at root that is a
        // {role: [model_id, ...]} map (written by the previous version of DjinnSettings).
        if let Some(arr) = v.get("model_priority").and_then(|mp| mp.as_array()) {
            // Flat list of model IDs (shouldn't normally exist, but handle defensively).
            let out: Vec<String> = arr
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect();
            if !out.is_empty() {
                return Some(out);
            }
        }

        let root = v
            .get("coordinator")
            .and_then(|c| c.get("model_priority"))
            .or_else(|| v.get("execution").and_then(|e| e.get("model_priority")))
            // Intermediate typed format: `model_priority` is a `{role: [model_id]}` map at root.
            .or_else(|| v.get("model_priority"))
            .or_else(|| v.get("models").and_then(|m| m.get("priority")))?
            .as_object()?;

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for value in root.values() {
            if let Some(arr) = value.as_array() {
                for model in arr.iter().filter_map(serde_json::Value::as_str) {
                    if seen.insert(model.to_owned()) {
                        out.push(model.to_owned());
                    }
                }
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }

    fn extract_max_sessions(v: &serde_json::Value) -> Option<HashMap<String, u32>> {
        let map = v
            .get("max_sessions")
            .or_else(|| v.get("execution").and_then(|e| e.get("max_sessions")))
            .or_else(|| v.get("supervisor").and_then(|s| s.get("max_sessions")))
            .and_then(serde_json::Value::as_object)?;

        let mut out = HashMap::new();
        for (model_id, max) in map {
            if let Some(max) = max.as_u64()
                && max > 0
            {
                out.insert(model_id.clone(), max as u32);
            }
        }
        if out.is_empty() { None } else { Some(out) }
    }

    pub fn dispatch_limit_or_default(&self) -> usize {
        self.dispatch_limit.unwrap_or(50) as usize
    }

    pub fn models_or_default(&self) -> Vec<String> {
        self.models.clone().unwrap_or_default()
    }

    pub fn max_sessions_or_default(&self) -> HashMap<String, u32> {
        self.max_sessions.clone().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_db_value_parses_typed_format() {
        let s = DjinnSettings::from_db_value(raw);
        assert_eq!(s.dispatch_limit, Some(100));
        assert_eq!(s.models.as_ref().unwrap(), &vec!["openai/gpt-4o"]);
        assert!(s.max_sessions.is_none());
    }

    #[test]
    fn from_db_value_migrates_legacy_format() {
        let raw = r#"{"coordinator":{"dispatch_limit":25,"model_priority":{"worker":["openai/gpt-4o"]}},"supervisor":{"max_sessions":3}}"#;
        let s = DjinnSettings::from_db_value(raw);
        assert_eq!(s.dispatch_limit, Some(25));
        assert_eq!(s.models.as_ref().unwrap(), &vec!["openai/gpt-4o"]);
        // Legacy scalar max_sessions is ignored (we only migrate map form)
        assert!(s.max_sessions.is_none());
    }

    #[test]
    fn from_db_value_falls_back_on_garbage() {
        let s = DjinnSettings::from_db_value("not json at all");
        assert_eq!(s.dispatch_limit, None);
    }

    #[test]
    fn deny_unknown_fields_rejects_unknown_keys() {
        let raw = r#"{"dispatch_limit":50,"bogus_key":true}"#;
        let result = serde_json::from_str::<DjinnSettings>(raw);
        assert!(result.is_err());
    }

    #[test]
    fn removed_langfuse_keys_are_stripped_not_migrated() {
        // DB rows written under the prior schema carried langfuse_* keys.
        // Ensure the strip path preserves the other typed fields instead of
        // falling through to the lossy legacy migrator (which would drop
        let s = DjinnSettings::from_db_value(raw);
        assert_eq!(s.dispatch_limit, Some(42));
    }

    #[test]
    fn defaults_are_correct() {
        let s = DjinnSettings::default();
        assert_eq!(s.dispatch_limit_or_default(), 50);
        assert!(s.models_or_default().is_empty());
        assert!(s.max_sessions_or_default().is_empty());
        assert!(s.dispatch_pause.is_none());
    }

    #[test]
    fn dispatch_pause_serializes_metadata_and_defaults_absent() {
        let raw = r#"{"dispatch_pause":{"paused_by":"admin","paused_at":"2026-06-12T00:00:00.000Z","reason":"maintenance"}}"#;
        let s = DjinnSettings::from_db_value(raw);
        let pause = s.dispatch_pause.expect("pause should parse");
        assert_eq!(pause.paused_by, "admin");
        assert_eq!(pause.paused_at, "2026-06-12T00:00:00.000Z");
        assert_eq!(pause.reason, "maintenance");
        assert!(pause.expires_at.is_none());

        let serialized = serde_json::to_value(&pause).unwrap();
        assert_eq!(serialized.get("paused_by").unwrap(), "admin");
        assert_eq!(
            serialized.get("paused_at").unwrap(),
            "2026-06-12T00:00:00.000Z"
        );
        assert_eq!(serialized.get("reason").unwrap(), "maintenance");
        assert!(serialized.get("expires_at").is_none());

        let older = DjinnSettings::from_db_value(r#"{"dispatch_limit":10}"#);
        assert!(older.dispatch_pause.is_none());
    }

    #[test]
    fn dispatch_pause_state_serializes_scoped_metadata() {
        let pause = DispatchPause {
            paused_by: "admin".to_owned(),
            paused_at: "2026-06-12T00:00:00.000Z".to_owned(),
            reason: "maintenance".to_owned(),
            expires_at: None,
        };
        let mut state = DispatchPauseState {
            global: Some(pause.clone()),
            projects: HashMap::new(),
            users: HashMap::new(),
        };
        state.projects.insert("project-1".to_owned(), pause.clone());
        state.users.insert("user-1".to_owned(), pause.clone());

        let serialized = serde_json::to_value(&state).unwrap();
        assert_eq!(serialized["global"]["paused_by"], "admin");
        assert_eq!(serialized["projects"]["project-1"]["reason"], "maintenance");
        assert_eq!(serialized["users"]["user-1"]["paused_at"], pause.paused_at);

        let parsed: DispatchPauseState = serde_json::from_value(serialized).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn dispatch_pause_scope_round_trips_as_typed_scope() {
        assert_eq!(DispatchPauseScope::Global.as_str(), "global");
        assert_eq!(DispatchPauseScope::Project.to_string(), "project");
        assert_eq!(
            "user".parse::<DispatchPauseScope>().unwrap(),
            DispatchPauseScope::User
        );
        assert!("bogus".parse::<DispatchPauseScope>().is_err());
    }

    #[test]
    fn legacy_max_sessions_map_is_migrated() {
        let raw = r#"{"max_sessions":{"openai/gpt-4o":4,"anthropic/claude-opus-4-6":2}}"#;
        let s = DjinnSettings::from_db_value(raw);
        let ms = s.max_sessions.unwrap();
        assert_eq!(ms.get("openai/gpt-4o"), Some(&4));
        assert_eq!(ms.get("anthropic/claude-opus-4-6"), Some(&2));
    }

    /// Old `DjinnSettings` struct had `model_priority: Option<HashMap<String, Vec<String>>>`.
    /// Validate that this intermediate typed format is correctly migrated to the new flat `models`
    /// list, so startups no longer emit the legacy-format warning for these DBs.
    #[test]
    fn intermediate_typed_format_model_priority_map_is_migrated() {
        let raw = r#"{"model_priority":{"worker":["openai/gpt-4o","anthropic/claude-opus-4-6"],"reviewer":["openai/gpt-4o"]},"max_sessions":{"openai/gpt-4o":2}}"#;
        let s = DjinnSettings::from_db_value(raw);
        // Should have extracted a deduplicated flat model list.
        let models = s
            .models
            .expect("models should be extracted from model_priority map");
        assert!(
            models.contains(&"openai/gpt-4o".to_string()),
            "gpt-4o should be in models"
        );
        assert!(
            models.contains(&"anthropic/claude-opus-4-6".to_string()),
            "claude should be in models"
        );
        // Deduplication: gpt-4o appears in both worker and reviewer roles — only once in output.
        assert_eq!(
            models
                .iter()
                .filter(|m| m.as_str() == "openai/gpt-4o")
                .count(),
            1
        );
        // max_sessions should be migrated too.
        let ms = s.max_sessions.expect("max_sessions should be migrated");
        assert_eq!(ms.get("openai/gpt-4o"), Some(&2));
    }

    /// Old format also had a `memory_model` field. Verify it is gracefully dropped during
    /// migration without causing a panic or losing other fields.
    #[test]
    fn intermediate_typed_format_with_memory_model_is_migrated() {
        let raw = r#"{"model_priority":{"worker":["openai/gpt-4o"]},"memory_model":"openai/gpt-4o-mini","max_sessions":{"openai/gpt-4o":1}}"#;
        let s = DjinnSettings::from_db_value(raw);
        // models extracted correctly
        let models = s.models.expect("models should be extracted");
        assert_eq!(models, vec!["openai/gpt-4o"]);
        // memory_model is silently dropped (not a field in the current schema)
        // max_sessions should still be migrated
        let ms = s.max_sessions.expect("max_sessions should be migrated");
        assert_eq!(ms.get("openai/gpt-4o"), Some(&1));
    }
}
