//! Deterministic normalized export for persisted model tool calls.
//!
//! Provider/catalog values are supplied by the caller because this crate must not
//! depend on `djinn-provider`; missing values stay null and are diagnosed.
use crate::{Database, Result};
use djinn_core::message::ContentBlock;
use djinn_core::models::{SessionMessage, SessionRecord};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExportDimensions {
    pub provider_id: Option<String>,
    pub format_family: Option<String>,
    pub tool_surface_family: Option<String>,
}
#[derive(Clone, Debug)]
pub struct PersistedTranscript {
    pub session: SessionRecord,
    pub messages: Vec<SessionMessage>,
    pub dimensions: ExportDimensions,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NormalizedToolCallRow {
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub format_family: Option<String>,
    pub tool_surface_family: Option<String>,
    pub agent_role: Option<String>,
    pub session_id: String,
    pub task_id: Option<String>,
    pub calendar_day: Option<String>,
    pub window_start: Option<String>,
    pub tool_call_id: Option<String>,
    pub turn_index: usize,
    pub tool_name: String,
    pub args_hash: String,
    pub result_status: String,
    pub error_class: Option<String>,
    pub error_text: Option<String>,
    pub read_truncated: bool,
    pub diagnostics: Vec<String>,
}
pub struct ToolCallExportRepository {
    db: Database,
}
impl ToolCallExportRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
    pub async fn export_session(
        &self,
        session: SessionRecord,
        dimensions: ExportDimensions,
    ) -> Result<Vec<NormalizedToolCallRow>> {
        self.db.ensure_initialized().await?;
        let messages = sqlx::query_as!(SessionMessage, r#"SELECT id, session_id, role, content_json::text AS "content_json!", token_count, created_at FROM session_messages WHERE session_id = $1 ORDER BY created_at ASC, id ASC"#, session.id).fetch_all(self.db.pool()).await?;
        Ok(normalize_persisted_transcript(&PersistedTranscript {
            session,
            messages,
            dimensions,
        }))
    }
}
fn present(s: String) -> Option<String> {
    (!s.trim().is_empty()).then_some(s)
}
fn opt(s: Option<String>) -> Option<String> {
    s.and_then(present)
}
pub fn stable_args_hash(v: &serde_json::Value) -> String {
    format!("sha256:{:x}", Sha256::digest(canonical(v)))
}
fn canonical(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => {
            let x: BTreeMap<_, _> = m.iter().collect();
            format!(
                "{{{}}}",
                x.into_iter()
                    .map(|(k, v)| format!("{}:{}", serde_json::to_string(k).unwrap(), canonical(v)))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        serde_json::Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical).collect::<Vec<_>>().join(",")
        ),
        _ => serde_json::to_string(v).unwrap(),
    }
}
fn trunc(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(m) => m.iter().any(|(k, v)| {
            ((k == "has_more" || k == "truncated" || k == "read_truncated")
                && v == &serde_json::Value::Bool(true))
                || trunc(v)
        }),
        serde_json::Value::Array(a) => a.iter().any(trunc),
        _ => false,
    }
}
fn err(text: &str) -> (&'static str, String) {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let l = t.to_ascii_lowercase();
    let c = if l.contains("cancel") || l.contains("interrupted") {
        "cancelled"
    } else if l.contains("timeout") {
        "timeout"
    } else if l.contains("provider") || l.contains("rate limit") || l.contains("http ") {
        "provider"
    } else {
        "tool"
    };
    (c, t.chars().take(512).collect())
}
pub fn normalize_persisted_transcript(input: &PersistedTranscript) -> Vec<NormalizedToolCallRow> {
    let mut calls = Vec::new();
    let mut results = Vec::new();
    for (pos, m) in input.messages.iter().enumerate() {
        if let Ok(blocks) = serde_json::from_str::<Vec<ContentBlock>>(&m.content_json) {
            for b in blocks {
                match b {
                    ContentBlock::ToolUse {
                        id,
                        name,
                        input: args,
                    } if m.role == "assistant" => calls.push((pos, present(id), name, args)),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        let text = content
                            .iter()
                            .filter_map(ContentBlock::as_text)
                            .collect::<Vec<_>>()
                            .join(" ");
                        let structured = serde_json::to_value(&content)
                            .map(|v| trunc(&v))
                            .unwrap_or(false);
                        results.push((pos, present(tool_use_id), is_error, text, structured));
                    }
                    _ => {}
                }
            }
        }
    }
    let mut used = vec![false; results.len()];
    calls
        .into_iter()
        .map(|(pos, id, name, args)| {
            let mut d = Vec::new();
            for (key, val) in [
                (
                    "missing_provider_id",
                    opt(input.dimensions.provider_id.clone()),
                ),
                ("missing_model_id", present(input.session.model_id.clone())),
                (
                    "missing_format_family",
                    opt(input.dimensions.format_family.clone()),
                ),
                (
                    "missing_tool_surface_family",
                    opt(input.dimensions.tool_surface_family.clone()),
                ),
                (
                    "missing_agent_role",
                    present(input.session.agent_type.clone()),
                ),
                ("missing_task_id", opt(input.session.task_id.clone())),
            ] {
                if val.is_none() {
                    d.push(key.to_owned())
                }
            }
            if id.is_none() {
                d.push("missing_tool_call_id".into())
            };
            let ri = if let Some(ref cid) = id {
                if let Some(i) = results
                    .iter()
                    .enumerate()
                    .find_map(|(i, r)| (!used[i] && r.1.as_ref() == Some(cid)).then_some(i))
                {
                    Some(i)
                } else if results.iter().any(|r| r.1.is_some()) {
                    d.push("missing_matching_tool_result".into());
                    None
                } else {
                    d.push("tool_result_paired_by_transcript_position".into());
                    results
                        .iter()
                        .enumerate()
                        .find_map(|(i, r)| (!used[i] && r.0 > pos).then_some(i))
                }
            } else {
                d.push("tool_result_paired_by_transcript_position".into());
                results
                    .iter()
                    .enumerate()
                    .find_map(|(i, r)| (!used[i] && r.0 > pos).then_some(i))
            };
            if let Some(i) = ri {
                used[i] = true
            };
            let r = ri.map(|i| &results[i]);
            let (status, ec, et, rt) = match r {
                None => ("missing".into(), None, None, false),
                Some(r) if !r.2 => (
                    "success".into(),
                    None,
                    None,
                    r.4 || r.3.to_ascii_lowercase().contains("truncated"),
                ),
                Some(r) => {
                    let (e, t) = err(&r.3);
                    (
                        "error".into(),
                        Some(e.into()),
                        Some(t),
                        r.4 || r.3.to_ascii_lowercase().contains("truncated"),
                    )
                }
            };
            d.sort();
            d.dedup();
            let day = (input.session.started_at.len() >= 10)
                .then(|| input.session.started_at[..10].to_owned());
            NormalizedToolCallRow {
                provider_id: opt(input.dimensions.provider_id.clone()),
                model_id: present(input.session.model_id.clone()),
                format_family: opt(input.dimensions.format_family.clone()),
                tool_surface_family: opt(input.dimensions.tool_surface_family.clone()),
                agent_role: present(input.session.agent_type.clone()),
                session_id: input.session.id.clone(),
                task_id: opt(input.session.task_id.clone()),
                window_start: day.as_ref().map(|x| format!("{x}T00:00:00Z")),
                calendar_day: day,
                tool_call_id: id,
                turn_index: pos,
                tool_name: name,
                args_hash: stable_args_hash(&args),
                result_status: status,
                error_class: ec,
                error_text: et,
                read_truncated: rt,
                diagnostics: d,
            }
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hash_is_key_order_stable() {
        assert_eq!(
            stable_args_hash(&serde_json::json!({"a":1,"b":2})),
            stable_args_hash(&serde_json::json!({"b":2,"a":1}))
        );
    }
    #[test]
    fn truncation_signals_are_detected() {
        assert!(trunc(&serde_json::json!({"nested":{"has_more":true}})));
        let (e, t) = err("cancelled\n x");
        assert_eq!(e, "cancelled");
        assert_eq!(t, "cancelled x");
    }
}
