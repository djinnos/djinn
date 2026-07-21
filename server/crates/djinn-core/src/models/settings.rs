use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Immutable process configuration for bounded knowledge injection and retrieval health.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnowledgeInjectionConfig {
    pub knowledge_injection_budget_bytes: u32,
    pub knowledge_injection_line_cap_bytes: u32,
    pub knowledge_injection_limit: u32,
    pub injection_starvation_threshold_percent: u32,
    pub injection_starvation_query_floor: u32,
    pub retrieval_health_window_minutes: u32,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("invalid {field} value `{value}`: {reason}")]
pub struct KnowledgeInjectionConfigError {
    pub field: &'static str,
    pub value: String,
    pub reason: String,
}

impl KnowledgeInjectionConfig {
    pub const DEFAULT_KNOWLEDGE_INJECTION_BUDGET_BYTES: u32 = 8192;
    pub const DEFAULT_KNOWLEDGE_INJECTION_LINE_CAP_BYTES: u32 = 1024;
    pub const DEFAULT_KNOWLEDGE_INJECTION_LIMIT: u32 = 10;
    pub const DEFAULT_INJECTION_STARVATION_THRESHOLD_PERCENT: u32 = 50;
    pub const DEFAULT_INJECTION_STARVATION_QUERY_FLOOR: u32 = 20;
    pub const DEFAULT_RETRIEVAL_HEALTH_WINDOW_MINUTES: u32 = 1440;
    pub fn from_settings_and_env(s: &DjinnSettings) -> Result<Self, KnowledgeInjectionConfigError> {
        // An environment override selects the effective value, but must not
        // conceal an invalid value that is already present in settings.raw.
        Self::from_settings(s)?;
        let budget = resolve(
            "knowledge_injection_budget_bytes",
            "DJINN_KNOWLEDGE_INJECTION_BUDGET_BYTES",
            s.knowledge_injection_budget_bytes,
            8192,
            256,
            32768,
        )?;
        let line_cap = resolve(
            "knowledge_injection_line_cap_bytes",
            "DJINN_KNOWLEDGE_INJECTION_LINE_CAP_BYTES",
            s.knowledge_injection_line_cap_bytes,
            1024,
            128,
            4096,
        )?;
        let limit = resolve(
            "knowledge_injection_limit",
            "DJINN_KNOWLEDGE_INJECTION_LIMIT",
            s.knowledge_injection_limit,
            10,
            1,
            50,
        )?;
        let threshold = resolve(
            "injection_starvation_threshold_percent",
            "DJINN_INJECTION_STARVATION_THRESHOLD_PERCENT",
            s.injection_starvation_threshold_percent,
            50,
            1,
            100,
        )?;
        let floor = resolve(
            "injection_starvation_query_floor",
            "DJINN_INJECTION_STARVATION_QUERY_FLOOR",
            s.injection_starvation_query_floor,
            20,
            1,
            10000,
        )?;
        let window = resolve(
            "retrieval_health_window_minutes",
            "DJINN_RETRIEVAL_HEALTH_WINDOW_MINUTES",
            s.retrieval_health_window_minutes,
            1440,
            5,
            10080,
        )?;
        if line_cap > budget {
            return Err(KnowledgeInjectionConfigError {
                field: "knowledge_injection_line_cap_bytes",
                value: format!("{line_cap} (knowledge_injection_budget_bytes={budget})"),
                reason: "must be less than or equal to knowledge_injection_budget_bytes".into(),
            });
        }
        Ok(Self {
            knowledge_injection_budget_bytes: budget,
            knowledge_injection_line_cap_bytes: line_cap,
            knowledge_injection_limit: limit,
            injection_starvation_threshold_percent: threshold,
            injection_starvation_query_floor: floor,
            retrieval_health_window_minutes: window,
        })
    }

    /// Resolve persisted settings without consulting environment overrides.
    /// Startup uses this before precedence so every present file value is
    /// independently validated.
    pub fn from_settings(s: &DjinnSettings) -> Result<Self, KnowledgeInjectionConfigError> {
        let budget = resolve_file(
            "knowledge_injection_budget_bytes",
            s.knowledge_injection_budget_bytes,
            8192,
            256,
            32768,
        )?;
        let line_cap = resolve_file(
            "knowledge_injection_line_cap_bytes",
            s.knowledge_injection_line_cap_bytes,
            1024,
            128,
            4096,
        )?;
        let limit = resolve_file(
            "knowledge_injection_limit",
            s.knowledge_injection_limit,
            10,
            1,
            50,
        )?;
        let threshold = resolve_file(
            "injection_starvation_threshold_percent",
            s.injection_starvation_threshold_percent,
            50,
            1,
            100,
        )?;
        let floor = resolve_file(
            "injection_starvation_query_floor",
            s.injection_starvation_query_floor,
            20,
            1,
            10000,
        )?;
        let window = resolve_file(
            "retrieval_health_window_minutes",
            s.retrieval_health_window_minutes,
            1440,
            5,
            10080,
        )?;
        validate_line_cap(line_cap, budget)?;
        Ok(Self {
            knowledge_injection_budget_bytes: budget,
            knowledge_injection_line_cap_bytes: line_cap,
            knowledge_injection_limit: limit,
            injection_starvation_threshold_percent: threshold,
            injection_starvation_query_floor: floor,
            retrieval_health_window_minutes: window,
        })
    }
}
impl Default for KnowledgeInjectionConfig {
    fn default() -> Self {
        Self::from_settings_and_env(&DjinnSettings::default()).expect("valid defaults")
    }
}
fn resolve(
    field: &'static str,
    env_var: &str,
    file: Option<u32>,
    default: u32,
    min: u32,
    max: u32,
) -> Result<u32, KnowledgeInjectionConfigError> {
    let (value, rendered) = match std::env::var(env_var) {
        Ok(raw) => (
            raw.parse().map_err(|_| KnowledgeInjectionConfigError {
                field,
                value: raw.clone(),
                reason: format!("{env_var} must be an integer"),
            })?,
            raw,
        ),
        Err(std::env::VarError::NotPresent) => {
            let value = file.unwrap_or(default);
            (value, value.to_string())
        }
        Err(std::env::VarError::NotUnicode(raw)) => {
            return Err(KnowledgeInjectionConfigError {
                field,
                value: raw.to_string_lossy().into_owned(),
                reason: format!("{env_var} is not valid Unicode"),
            });
        }
    };
    if !(min..=max).contains(&value) {
        return Err(KnowledgeInjectionConfigError {
            field,
            value: rendered,
            reason: format!("must be in [{min}, {max}]"),
        });
    }
    Ok(value)
}

fn resolve_file(
    field: &'static str,
    value: Option<u32>,
    default: u32,
    min: u32,
    max: u32,
) -> Result<u32, KnowledgeInjectionConfigError> {
    let value = value.unwrap_or(default);
    if !(min..=max).contains(&value) {
        return Err(KnowledgeInjectionConfigError {
            field,
            value: value.to_string(),
            reason: format!("must be in [{min}, {max}]"),
        });
    }
    Ok(value)
}

fn validate_line_cap(line_cap: u32, budget: u32) -> Result<(), KnowledgeInjectionConfigError> {
    if line_cap > budget {
        return Err(KnowledgeInjectionConfigError {
            field: "knowledge_injection_line_cap_bytes",
            value: format!("{line_cap} (knowledge_injection_budget_bytes={budget})"),
            reason: "must be less than or equal to knowledge_injection_budget_bytes".into(),
        });
    }
    Ok(())
}

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
    /// Maximum total UTF-8 bytes injected from retrieved knowledge (default 8192 bytes).
    pub knowledge_injection_budget_bytes: Option<u32>,
    /// Maximum UTF-8 bytes used for one injected knowledge summary (default 1024 bytes).
    pub knowledge_injection_line_cap_bytes: Option<u32>,
    /// Maximum retrieved knowledge candidates considered for injection (default 10).
    pub knowledge_injection_limit: Option<u32>,
    /// Injection-starvation alert threshold in percent (default 50 percent).
    pub injection_starvation_threshold_percent: Option<u32>,
    /// Minimum query count for injection-starvation evaluation (default 20 queries).
    pub injection_starvation_query_floor: Option<u32>,
    /// Retrieval-health aggregation window in minutes (default 1440 minutes).
    pub retrieval_health_window_minutes: Option<u32>,
}

impl DjinnSettings {
    /// Deserialize persisted settings while refusing to silently migrate a
    /// malformed canonical knowledge-injection field. Older, unrelated
    /// settings formats retain the historical best-effort migration path.
    pub fn from_db_value_validated(raw: &str) -> Result<Self, KnowledgeInjectionConfigError> {
        let cleaned = Self::strip_removed_fields(raw);
        let input = cleaned.as_deref().unwrap_or(raw);
        match serde_json::from_str::<Self>(input) {
            Ok(settings) => {
                KnowledgeInjectionConfig::from_settings(&settings)?;
                Ok(settings)
            }
            Err(_) => {
                if let Some((field, value)) = Self::invalid_canonical_config_value(raw) {
                    return Err(KnowledgeInjectionConfigError {
                        field,
                        value,
                        reason: "must be an integer".into(),
                    });
                }
                Ok(Self::from_db_value(raw))
            }
        }
    }

    fn invalid_canonical_config_value(raw: &str) -> Option<(&'static str, String)> {
        const FIELDS: &[&str] = &[
            "knowledge_injection_budget_bytes",
            "knowledge_injection_line_cap_bytes",
            "knowledge_injection_limit",
            "injection_starvation_threshold_percent",
            "injection_starvation_query_floor",
            "retrieval_health_window_minutes",
        ];
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;
        let object = value.as_object()?;
        FIELDS.iter().find_map(|field| {
            object
                .get(*field)
                .filter(|value| value.as_u64().is_none_or(|value| value > u32::MAX as u64))
                .map(|value| {
                    (
                        *field,
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string()),
                    )
                })
        })
    }

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
            knowledge_injection_budget_bytes: None,
            knowledge_injection_line_cap_bytes: None,
            knowledge_injection_limit: None,
            injection_starvation_threshold_percent: None,
            injection_starvation_query_floor: None,
            retrieval_health_window_minutes: None,
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
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    const CONFIG_ENV: &[&str] = &[
        "DJINN_KNOWLEDGE_INJECTION_BUDGET_BYTES",
        "DJINN_KNOWLEDGE_INJECTION_LINE_CAP_BYTES",
        "DJINN_KNOWLEDGE_INJECTION_LIMIT",
        "DJINN_INJECTION_STARVATION_THRESHOLD_PERCENT",
        "DJINN_INJECTION_STARVATION_QUERY_FLOOR",
        "DJINN_RETRIEVAL_HEALTH_WINDOW_MINUTES",
    ];

    fn without_config_env(test: impl FnOnce()) {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let saved: Vec<_> = CONFIG_ENV
            .iter()
            .map(|key| (*key, std::env::var(key).ok()))
            .collect();
        for key in CONFIG_ENV {
            unsafe { std::env::remove_var(key) };
        }
        test();
        for (key, value) in saved {
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }

    #[test]
    fn knowledge_injection_config_defaults_file_and_environment_precedence() {
        without_config_env(|| {
            let defaults =
                KnowledgeInjectionConfig::from_settings_and_env(&DjinnSettings::default()).unwrap();
            assert_eq!(
                (
                    defaults.knowledge_injection_budget_bytes,
                    defaults.knowledge_injection_line_cap_bytes,
                    defaults.knowledge_injection_limit,
                    defaults.injection_starvation_threshold_percent,
                    defaults.injection_starvation_query_floor,
                    defaults.retrieval_health_window_minutes
                ),
                (8192, 1024, 10, 50, 20, 1440)
            );
            let settings: DjinnSettings = serde_json::from_str(r#"{"knowledge_injection_budget_bytes":9000,"knowledge_injection_line_cap_bytes":1000,"knowledge_injection_limit":11,"injection_starvation_threshold_percent":51,"injection_starvation_query_floor":21,"retrieval_health_window_minutes":60}"#).unwrap();
            let config = KnowledgeInjectionConfig::from_settings_and_env(&settings).unwrap();
            assert_eq!(
                (
                    config.knowledge_injection_budget_bytes,
                    config.knowledge_injection_line_cap_bytes,
                    config.knowledge_injection_limit,
                    config.injection_starvation_threshold_percent,
                    config.injection_starvation_query_floor,
                    config.retrieval_health_window_minutes
                ),
                (9000, 1000, 11, 51, 21, 60)
            );
            unsafe { std::env::set_var("DJINN_KNOWLEDGE_INJECTION_BUDGET_BYTES", "10000") };
            assert_eq!(
                KnowledgeInjectionConfig::from_settings_and_env(&settings)
                    .unwrap()
                    .knowledge_injection_budget_bytes,
                10000
            );
        });
    }

    #[test]
    fn knowledge_injection_config_accepts_all_inclusive_bounds() {
        without_config_env(|| {
            for (field, min, max) in [
                ("knowledge_injection_budget_bytes", 256, 32768),
                ("knowledge_injection_line_cap_bytes", 128, 4096),
                ("knowledge_injection_limit", 1, 50),
                ("injection_starvation_threshold_percent", 1, 100),
                ("injection_starvation_query_floor", 1, 10000),
                ("retrieval_health_window_minutes", 5, 10080),
            ] {
                for value in [min, max] {
                    let mut settings = DjinnSettings {
                        knowledge_injection_budget_bytes: Some(
                            if field == "knowledge_injection_budget_bytes" && value == 256 {
                                256
                            } else {
                                8192
                            },
                        ),
                        knowledge_injection_line_cap_bytes: Some(
                            if field == "knowledge_injection_budget_bytes" && value == 256 {
                                256
                            } else {
                                1024
                            },
                        ),
                        ..Default::default()
                    };
                    match field {
                        "knowledge_injection_budget_bytes" => {
                            settings.knowledge_injection_budget_bytes = Some(value)
                        }
                        "knowledge_injection_line_cap_bytes" => {
                            settings.knowledge_injection_line_cap_bytes = Some(value)
                        }
                        "knowledge_injection_limit" => {
                            settings.knowledge_injection_limit = Some(value)
                        }
                        "injection_starvation_threshold_percent" => {
                            settings.injection_starvation_threshold_percent = Some(value)
                        }
                        "injection_starvation_query_floor" => {
                            settings.injection_starvation_query_floor = Some(value)
                        }
                        "retrieval_health_window_minutes" => {
                            settings.retrieval_health_window_minutes = Some(value)
                        }
                        _ => unreachable!(),
                    }
                    assert!(
                        KnowledgeInjectionConfig::from_settings_and_env(&settings).is_ok(),
                        "{field}={value}"
                    );
                }
            }
        });
    }

    #[test]
    fn knowledge_injection_config_rejects_invalid_and_cross_field_values() {
        without_config_env(|| {
            unsafe { std::env::set_var("DJINN_KNOWLEDGE_INJECTION_LIMIT", "nope") };
            let error = KnowledgeInjectionConfig::from_settings_and_env(&DjinnSettings::default())
                .unwrap_err()
                .to_string();
            assert!(error.contains("knowledge_injection_limit") && error.contains("nope"));
            unsafe { std::env::remove_var("DJINN_KNOWLEDGE_INJECTION_LIMIT") };
            let invalid = DjinnSettings {
                knowledge_injection_budget_bytes: Some(256),
                knowledge_injection_line_cap_bytes: Some(257),
                ..Default::default()
            };
            let error = KnowledgeInjectionConfig::from_settings_and_env(&invalid)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("knowledge_injection_line_cap_bytes")
                    && error.contains("257")
                    && error.contains("knowledge_injection_budget_bytes")
                    && error.contains("256")
            );
            let equal = DjinnSettings {
                knowledge_injection_line_cap_bytes: Some(256),
                ..invalid
            };
            assert!(KnowledgeInjectionConfig::from_settings_and_env(&equal).is_ok());
            unsafe {
                std::env::set_var("DJINN_KNOWLEDGE_INJECTION_BUDGET_BYTES", "8192");
                std::env::set_var("DJINN_KNOWLEDGE_INJECTION_LINE_CAP_BYTES", "1024");
            }
            let error = DjinnSettings::from_db_value_validated(
                r#"{"knowledge_injection_budget_bytes":256,"knowledge_injection_line_cap_bytes":257}"#,
            )
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("knowledge_injection_line_cap_bytes")
                    && error.contains("257")
                    && error.contains("knowledge_injection_budget_bytes")
                    && error.contains("256")
            );
            let error = DjinnSettings::from_db_value_validated(
                r#"{"knowledge_injection_budget_bytes":"bad"}"#,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("knowledge_injection_budget_bytes") && error.contains("bad"));
            let error = DjinnSettings::from_db_value_validated(
                r#"{"knowledge_injection_budget_bytes":255}"#,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("knowledge_injection_budget_bytes") && error.contains("255"));
        });
    }

    #[test]
    fn from_db_value_parses_typed_format() {
        let raw = r#"{"dispatch_limit":100,"models":["openai/gpt-4o"]}"#;
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
        // them).
        let raw = r#"{"dispatch_limit":42,"langfuse_public_key":"pk","langfuse_secret_key":"sk","langfuse_endpoint":"http://x"}"#;
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
