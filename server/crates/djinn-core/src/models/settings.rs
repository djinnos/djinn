use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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
    /// Per-model concurrent session caps, e.g. `{"openai/gpt-4o": 4}`.
    #[schemars(with = "Option<HashMap<String, i64>>")]
    pub max_sessions: Option<HashMap<String, u32>>,
    /// Langfuse public key for OTLP trace export (e.g. `pk-lf-...`).
    pub langfuse_public_key: Option<String>,
    /// Langfuse secret key for OTLP trace export (e.g. `sk-lf-...`).
    pub langfuse_secret_key: Option<String>,
    /// Langfuse OTLP endpoint URL (defaults to `http://localhost:3000/api/public/otel`).
    pub langfuse_endpoint: Option<String>,
    /// Enable the ADR-057 Linux memory mount. Disabled by default.
    pub memory_mount_enabled: Option<bool>,
    /// Absolute filesystem path where the Linux FUSE mount should be attached.
    pub memory_mount_path: Option<String>,
}

impl DjinnSettings {
    /// Deserialize from a raw DB value string, tolerating old/invalid formats
    /// by falling back to defaults with a warning.
    pub fn from_db_value(raw: &str) -> Self {
        match serde_json::from_str::<Self>(raw) {
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
            langfuse_public_key: None,
            langfuse_secret_key: None,
            langfuse_endpoint: None,
            memory_mount_enabled: None,
            memory_mount_path: None,
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
        let raw = r#"{"dispatch_limit":100,"models":["openai/gpt-4o"],"memory_mount_enabled":true,"memory_mount_path":"/tmp/djinn-memory"}"#;
        let s = DjinnSettings::from_db_value(raw);
        assert_eq!(s.dispatch_limit, Some(100));
        assert_eq!(s.models.as_ref().unwrap(), &vec!["openai/gpt-4o"]);
        assert!(s.max_sessions.is_none());
        assert_eq!(s.memory_mount_enabled, Some(true));
        assert_eq!(s.memory_mount_path.as_deref(), Some("/tmp/djinn-memory"));
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
    fn defaults_are_correct() {
        let s = DjinnSettings::default();
        assert_eq!(s.dispatch_limit_or_default(), 50);
        assert!(s.models_or_default().is_empty());
        assert!(s.max_sessions_or_default().is_empty());
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
