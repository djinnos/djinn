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

    fn session(
        id: &str,
        model_id: &str,
        agent_type: &str,
        task_id: Option<&str>,
        started_at: &str,
    ) -> SessionRecord {
        SessionRecord {
            id: id.into(),
            project_id: None,
            task_id: task_id.map(str::to_owned),
            model_id: model_id.into(),
            agent_type: agent_type.into(),
            started_at: started_at.into(),
            ended_at: None,
            status: "completed".into(),
            tokens_in: 0,
            tokens_out: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            task_run_id: None,
            title: None,
            parked_reason: None,
            cost_usd: None,
            input_price_per_million_snapshot: None,
            output_price_per_million_snapshot: None,
            cache_read_price_per_million_snapshot: None,
            cache_write_price_per_million_snapshot: None,
            cost_basis: "unpriced".into(),
            billing_source: None,
        }
    }

    fn message(position: usize, role: &str, content: serde_json::Value) -> SessionMessage {
        SessionMessage {
            id: format!("message-{position}"),
            session_id: "session-1".into(),
            role: role.into(),
            content_json: content.to_string(),
            token_count: None,
            created_at: format!("2026-02-03T00:00:{position:02}Z"),
        }
    }

    fn dimensions() -> ExportDimensions {
        ExportDimensions {
            provider_id: Some("anthropic".into()),
            format_family: Some("anthropic_messages".into()),
            tool_surface_family: Some("native".into()),
        }
    }
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

    #[test]
    fn persisted_transcript_fixture_pairs_calls_and_normalizes_complete_rows() {
        // Ordering has an unrelated pre-call result and reverse-ordered identified results.
        let transcript = PersistedTranscript {
            session: session(
                "session-1",
                "claude-test",
                "worker",
                Some("task-1"),
                "2026-02-03T04:05:06Z",
            ),
            dimensions: dimensions(),
            messages: vec![
                message(
                    0,
                    "user",
                    serde_json::json!([{"type":"tool_result","tool_use_id":"unrelated","content":[{"type":"text","text":"ignore me"}],"is_error":false}]),
                ),
                message(
                    1,
                    "assistant",
                    serde_json::json!([{"type":"tool_use","id":"call-a","name":"read","input":{"path":"src/lib.rs"}}]),
                ),
                message(
                    2,
                    "assistant",
                    serde_json::json!([{"type":"tool_use","id":"call-b","name":"apply_patch","input":{"patch":"x"}}]),
                ),
                message(
                    3,
                    "user",
                    serde_json::json!([{"type":"tool_result","tool_use_id":"call-b","content":[{"type":"text","text":"Provider rate limit"},{"type":"unknown","content_type":"provider_payload","has_more":true}],"is_error":true}]),
                ),
                message(
                    4,
                    "user",
                    serde_json::json!([{"type":"tool_result","tool_use_id":"call-a","content":[{"type":"text","text":"output was truncated"}],"is_error":false}]),
                ),
                message(
                    5,
                    "assistant",
                    serde_json::json!([{"type":"tool_use","id":"call-missing","name":"write","input":{}}]),
                ),
                message(
                    6,
                    "assistant",
                    serde_json::json!([{"type":"tool_use","id":"","name":"shell","input":{"command":"sleep"}}]),
                ),
                message(
                    7,
                    "user",
                    serde_json::json!([{"type":"tool_result","tool_use_id":"","content":[{"type":"text","text":"cancelled by user"}],"is_error":true}]),
                ),
            ],
        };
        let row = |tool_call_id: Option<String>,
                   turn_index: usize,
                   tool_name: &str,
                   args_hash: &str,
                   result_status: &str,
                   error_class: Option<String>,
                   error_text: Option<String>,
                   read_truncated: bool,
                   diagnostics: Vec<String>| NormalizedToolCallRow {
            provider_id: Some("anthropic".into()),
            model_id: Some("claude-test".into()),
            format_family: Some("anthropic_messages".into()),
            tool_surface_family: Some("native".into()),
            agent_role: Some("worker".into()),
            session_id: "session-1".into(),
            task_id: Some("task-1".into()),
            calendar_day: Some("2026-02-03".into()),
            window_start: Some("2026-02-03T00:00:00Z".into()),
            tool_call_id,
            turn_index,
            tool_name: tool_name.into(),
            args_hash: args_hash.into(),
            result_status: result_status.into(),
            error_class,
            error_text,
            read_truncated,
            diagnostics,
        };
        assert_eq!(
            normalize_persisted_transcript(&transcript),
            vec![
                row(
                    Some("call-a".into()),
                    1,
                    "read",
                    "sha256:54047b442992a19c4f9c11c7c70f2fe9a8344276b07cdbe6b65c218cffa37ecd",
                    "success",
                    None,
                    None,
                    true,
                    vec![]
                ),
                row(
                    Some("call-b".into()),
                    2,
                    "apply_patch",
                    "sha256:a23f7741869867d034a8f266ef420b0dff344e51a78d3f9cff0314826ce1c084",
                    "error",
                    Some("provider".into()),
                    Some("Provider rate limit".into()),
                    true,
                    vec![]
                ),
                row(
                    Some("call-missing".into()),
                    5,
                    "write",
                    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
                    "missing",
                    None,
                    None,
                    false,
                    vec!["missing_matching_tool_result".into()]
                ),
                row(
                    None,
                    6,
                    "shell",
                    "sha256:ca47c539409bc50021fbcfdfd5991b9a5eaf304e39c62fe6715f643bf5fa0ead",
                    "error",
                    Some("cancelled".into()),
                    Some("cancelled by user".into()),
                    false,
                    vec![
                        "missing_tool_call_id".into(),
                        "tool_result_paired_by_transcript_position".into()
                    ]
                ),
            ]
        );
    }

    #[test]
    fn persisted_fixture_keeps_missing_dimensions_null_and_bounds_timeout_error() {
        let error = format!(" timeout\n {}", "x".repeat(600));
        let transcript = PersistedTranscript {
            session: session("session-missing", "", "", None, ""),
            dimensions: ExportDimensions::default(),
            messages: vec![
                message(
                    0,
                    "assistant",
                    serde_json::json!([{"type":"tool_use","id":"call-timeout","name":"shell","input":{}}]),
                ),
                message(
                    1,
                    "user",
                    serde_json::json!([{"type":"tool_result","tool_use_id":"call-timeout","content":[{"type":"text","text":error}],"is_error":true}]),
                ),
            ],
        };
        let expected_error = format!("timeout {}", "x".repeat(600))
            .chars()
            .take(512)
            .collect();
        assert_eq!(
            normalize_persisted_transcript(&transcript),
            vec![NormalizedToolCallRow {
                provider_id: None,
                model_id: None,
                format_family: None,
                tool_surface_family: None,
                agent_role: None,
                session_id: "session-missing".into(),
                task_id: None,
                calendar_day: None,
                window_start: None,
                tool_call_id: Some("call-timeout".into()),
                turn_index: 0,
                tool_name: "shell".into(),
                args_hash:
                    "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a".into(),
                result_status: "error".into(),
                error_class: Some("timeout".into()),
                error_text: Some(expected_error),
                read_truncated: false,
                diagnostics: vec![
                    "missing_agent_role".into(),
                    "missing_format_family".into(),
                    "missing_model_id".into(),
                    "missing_provider_id".into(),
                    "missing_task_id".into(),
                    "missing_tool_surface_family".into()
                ],
            }]
        );
    }
}
