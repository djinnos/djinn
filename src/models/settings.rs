use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A key-value setting persisted in the `settings` table.
#[derive(Clone, Debug, Serialize, Deserialize, sqlx::FromRow)]
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
    /// Per-role ordered model lists, e.g. `{"worker": ["openai/gpt-4o"]}`.
    pub model_priority: Option<HashMap<String, Vec<String>>>,
    /// Per-model concurrent session caps, e.g. `{"openai/gpt-4o": 4}`.
    #[schemars(with = "Option<HashMap<String, i64>>")]
    pub max_sessions: Option<HashMap<String, u32>>,
    /// Helicone dev proxy base URL (e.g. `http://localhost:8585`). When set, all
    /// LLM requests route through the local Helicone gateway for full observability.
    pub dev_proxy_url: Option<String>,
    /// Helicone API key for the dev proxy (sent as `Helicone-Auth: Bearer {key}`).
    pub dev_proxy_key: Option<String>,
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

        let model_priority = Self::extract_model_priority(v);
        let max_sessions = Self::extract_max_sessions(v);

        Self {
            dispatch_limit,
            model_priority,
            max_sessions,
            dev_proxy_url: None,
            dev_proxy_key: None,
        }
    }

    fn extract_model_priority(v: &serde_json::Value) -> Option<HashMap<String, Vec<String>>> {
        let root = v
            .get("coordinator")
            .and_then(|c| c.get("model_priority"))
            .or_else(|| v.get("execution").and_then(|e| e.get("model_priority")))
            .or_else(|| v.get("models").and_then(|m| m.get("priority")))?
            .as_object()?;

        let mut out = HashMap::new();
        for (role, value) in root {
            if let Some(arr) = value.as_array() {
                let models: Vec<String> = arr
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect();
                if !models.is_empty() {
                    out.insert(role.clone(), models);
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

    pub fn model_priority_or_default(&self) -> HashMap<String, Vec<String>> {
        self.model_priority.clone().unwrap_or_default()
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
        let raw = r#"{"dispatch_limit":100,"model_priority":{"worker":["openai/gpt-4o"]}}"#;
        let s = DjinnSettings::from_db_value(raw);
        assert_eq!(s.dispatch_limit, Some(100));
        assert_eq!(
            s.model_priority.as_ref().unwrap().get("worker").unwrap(),
            &vec!["openai/gpt-4o"]
        );
        assert!(s.max_sessions.is_none());
    }

    #[test]
    fn from_db_value_migrates_legacy_format() {
        let raw = r#"{"coordinator":{"dispatch_limit":25,"model_priority":{"worker":["openai/gpt-4o"]}},"supervisor":{"max_sessions":3}}"#;
        let s = DjinnSettings::from_db_value(raw);
        assert_eq!(s.dispatch_limit, Some(25));
        assert_eq!(
            s.model_priority.as_ref().unwrap().get("worker").unwrap(),
            &vec!["openai/gpt-4o"]
        );
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
        assert!(s.model_priority_or_default().is_empty());
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
}
