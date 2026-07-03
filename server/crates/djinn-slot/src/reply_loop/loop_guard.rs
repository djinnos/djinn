//! Deterministic repeat-signature guard primitives for the reply loop.
//!
//! This module intentionally contains no provider, database, or tool-dispatch
//! dependencies. Later reply-loop wiring can feed it observed assistant turns
//! and tool outcomes, while unit tests can exercise the same pure transitions.
//!
//! Adapted from the agent implementation; depends only on `djinn-provider`
//! message types and `serde`/`sha2` (no `djinn-agent` imports).

use std::collections::HashMap;
use std::fmt;

use djinn_provider::message::ContentBlock;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Default number of identical failing `(tool_name, normalized_args)` attempts
/// before the general repeated-tool-failure guard trips.
pub const DEFAULT_IDENTICAL_TOOL_FAILURE_THRESHOLD: u32 = 3;

/// Default number of identical permission/security denials before the stricter denial guard trips.
pub const DEFAULT_PERMISSION_DENIAL_THRESHOLD: u32 = 2;

/// Default number of identical assistant output signatures before the assistant
/// output repeat guard trips.
pub const DEFAULT_ASSISTANT_OUTPUT_REPEAT_THRESHOLD: u32 = 4;

/// Default number of consecutive tool failures, across signatures, before the
/// consecutive-failure guard trips.
pub const DEFAULT_CONSECUTIVE_TOOL_FAILURE_THRESHOLD: u32 = 6;

/// Configurable guard thresholds. Defaults are the epic-wide values above.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoopGuardConfig {
    pub identical_tool_failure_threshold: u32,
    pub permission_denial_threshold: u32,
    pub assistant_output_repeat_threshold: u32,
    pub consecutive_tool_failure_threshold: u32,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            identical_tool_failure_threshold: DEFAULT_IDENTICAL_TOOL_FAILURE_THRESHOLD,
            permission_denial_threshold: DEFAULT_PERMISSION_DENIAL_THRESHOLD,
            assistant_output_repeat_threshold: DEFAULT_ASSISTANT_OUTPUT_REPEAT_THRESHOLD,
            consecutive_tool_failure_threshold: DEFAULT_CONSECUTIVE_TOOL_FAILURE_THRESHOLD,
        }
    }
}

/// Deterministic signature for one tool invocation, excluding provider-assigned
/// tool-call IDs and other runtime-random data.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ToolCallSignature {
    pub tool_name: String,
    pub normalized_args: String,
    pub digest: String,
}

impl ToolCallSignature {
    pub fn new(tool_name: impl Into<String>, args: &Value) -> Self {
        let tool_name = tool_name.into();
        let normalized_args = normalize_json(args);
        let digest = stable_digest(&["tool", &tool_name, &normalized_args]);
        Self {
            tool_name,
            normalized_args,
            digest,
        }
    }
    pub fn display_label(&self) -> String {
        format!(
            "{}({}) [digest={}]",
            self.tool_name, self.normalized_args, self.digest
        )
    }
}

/// Deterministic signature for one assistant turn: narrative text plus the
/// ordered tool-call signatures observed in that turn.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct AssistantOutputSignature {
    pub narrative_text: String,
    pub tool_calls: Vec<ToolCallSignature>,
    pub digest: String,
}

impl AssistantOutputSignature {
    pub fn new(
        narrative_text: impl Into<String>,
        tool_calls: impl IntoIterator<Item = ToolCallSignature>,
    ) -> Self {
        let narrative_text = narrative_text.into();
        let tool_calls: Vec<ToolCallSignature> = tool_calls.into_iter().collect();
        let mut parts = Vec::with_capacity(2 + tool_calls.len() * 3);
        parts.push("assistant".to_string());
        parts.push(narrative_text.clone());
        for call in &tool_calls {
            parts.push(call.tool_name.clone());
            parts.push(call.normalized_args.clone());
            parts.push(call.digest.clone());
        }
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        let digest = stable_digest(&refs);
        Self {
            narrative_text,
            tool_calls,
            digest,
        }
    }
    pub fn from_content_blocks(narrative_text: impl Into<String>, blocks: &[ContentBlock]) -> Self {
        let tool_calls = blocks.iter().filter_map(|block| match block {
            ContentBlock::ToolUse { name, input, .. } => Some(ToolCallSignature::new(name, input)),
            _ => None,
        });
        Self::new(narrative_text, tool_calls)
    }
}

/// Tool-failure class supplied by the future integration layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolFailureClass {
    General,
    PermissionOrSecurityDenial,
}

/// Coarse guard discriminator for downstream typed handling.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopGuardKind {
    RepeatedToolFailure,
    RepeatedPermissionOrSecurityDenial,
    RepeatedAssistantOutput,
    ConsecutiveToolFailures,
}

/// Typed reason payload that preserves the offending deterministic signature.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoopGuardReason {
    RepeatedToolFailure { signature: ToolCallSignature },
    RepeatedPermissionOrSecurityDenial { signature: ToolCallSignature },
    RepeatedAssistantOutput { signature: AssistantOutputSignature },
    ConsecutiveToolFailures { last_signature: ToolCallSignature },
}

impl LoopGuardReason {
    pub fn kind(&self) -> LoopGuardKind {
        match self {
            Self::RepeatedToolFailure { .. } => LoopGuardKind::RepeatedToolFailure,
            Self::RepeatedPermissionOrSecurityDenial { .. } => {
                LoopGuardKind::RepeatedPermissionOrSecurityDenial
            }
            Self::RepeatedAssistantOutput { .. } => LoopGuardKind::RepeatedAssistantOutput,
            Self::ConsecutiveToolFailures { .. } => LoopGuardKind::ConsecutiveToolFailures,
        }
    }
}

/// A tripped guard condition with observed counter state and threshold.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopGuardCondition {
    pub reason: LoopGuardReason,
    pub observed: u32,
    pub threshold: u32,
}

impl LoopGuardCondition {
    pub fn kind(&self) -> LoopGuardKind {
        self.reason.kind()
    }
    pub fn offending_signature_label(&self) -> String {
        match &self.reason {
            LoopGuardReason::RepeatedToolFailure { signature }
            | LoopGuardReason::RepeatedPermissionOrSecurityDenial { signature } => {
                signature.display_label()
            }
            LoopGuardReason::RepeatedAssistantOutput { signature } => {
                format!("assistant_output(digest={})", signature.digest)
            }
            LoopGuardReason::ConsecutiveToolFailures { last_signature } => {
                last_signature.display_label()
            }
        }
    }
}

/// Typed reply-loop error used when a deterministic loop guard terminates the session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopGuardError {
    pub condition: LoopGuardCondition,
    pub turn_span: (u32, u32),
    pub session_id: String,
}

impl fmt::Display for LoopGuardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "loop guard tripped: {:?}; offending signature: {}; observed {}/{}; turn_span={:?}; session_id={}",
            self.condition.kind(),
            self.condition.offending_signature_label(),
            self.condition.observed,
            self.condition.threshold,
            self.turn_span,
            self.session_id
        )
    }
}

impl std::error::Error for LoopGuardError {}

/// Pure in-memory guard state for one reply loop.
#[derive(Clone, Debug)]
pub struct LoopGuardState {
    config: LoopGuardConfig,
    tool_failure_counts: HashMap<ToolCallSignature, u32>,
    permission_denial_counts: HashMap<ToolCallSignature, u32>,
    assistant_output_counts: HashMap<AssistantOutputSignature, u32>,
    consecutive_tool_failures: u32,
}

impl Default for LoopGuardState {
    fn default() -> Self {
        Self::new(LoopGuardConfig::default())
    }
}

impl LoopGuardState {
    pub fn new(config: LoopGuardConfig) -> Self {
        Self {
            config,
            tool_failure_counts: HashMap::new(),
            permission_denial_counts: HashMap::new(),
            assistant_output_counts: HashMap::new(),
            consecutive_tool_failures: 0,
        }
    }
    pub fn record_assistant_output(
        &mut self,
        signature: AssistantOutputSignature,
    ) -> Option<LoopGuardCondition> {
        let observed = increment_count(&mut self.assistant_output_counts, signature.clone());
        (observed >= self.config.assistant_output_repeat_threshold).then_some(LoopGuardCondition {
            reason: LoopGuardReason::RepeatedAssistantOutput { signature },
            observed,
            threshold: self.config.assistant_output_repeat_threshold,
        })
    }
    pub fn record_tool_failure(
        &mut self,
        signature: ToolCallSignature,
        class: ToolFailureClass,
    ) -> Option<LoopGuardCondition> {
        self.consecutive_tool_failures = self.consecutive_tool_failures.saturating_add(1);
        let repeated_condition = self.record_identical_tool_failure(signature.clone(), class);
        let observed = self.consecutive_tool_failures;
        repeated_condition.or_else(|| {
            (observed >= self.config.consecutive_tool_failure_threshold).then_some(
                LoopGuardCondition {
                    reason: LoopGuardReason::ConsecutiveToolFailures {
                        last_signature: signature,
                    },
                    observed,
                    threshold: self.config.consecutive_tool_failure_threshold,
                },
            )
        })
    }
    pub fn record_tool_failure_after_progress(
        &mut self,
        signature: ToolCallSignature,
        class: ToolFailureClass,
    ) -> Option<LoopGuardCondition> {
        self.record_identical_tool_failure(signature, class)
    }
    fn record_identical_tool_failure(
        &mut self,
        signature: ToolCallSignature,
        class: ToolFailureClass,
    ) -> Option<LoopGuardCondition> {
        match class {
            ToolFailureClass::General => {
                let observed = increment_count(&mut self.tool_failure_counts, signature.clone());
                (observed >= self.config.identical_tool_failure_threshold).then(|| {
                    LoopGuardCondition {
                        reason: LoopGuardReason::RepeatedToolFailure {
                            signature: signature.clone(),
                        },
                        observed,
                        threshold: self.config.identical_tool_failure_threshold,
                    }
                })
            }
            ToolFailureClass::PermissionOrSecurityDenial => {
                let observed =
                    increment_count(&mut self.permission_denial_counts, signature.clone());
                (observed >= self.config.permission_denial_threshold).then(|| LoopGuardCondition {
                    reason: LoopGuardReason::RepeatedPermissionOrSecurityDenial {
                        signature: signature.clone(),
                    },
                    observed,
                    threshold: self.config.permission_denial_threshold,
                })
            }
        }
    }
    pub fn record_tool_success(&mut self) {
        self.tool_failure_counts.clear();
        self.permission_denial_counts.clear();
        self.assistant_output_counts.clear();
        self.consecutive_tool_failures = 0;
    }
    pub fn consecutive_tool_failures(&self) -> u32 {
        self.consecutive_tool_failures
    }
}

fn increment_count<K>(counts: &mut HashMap<K, u32>, key: K) -> u32
where
    K: std::hash::Hash + Eq,
{
    let count = counts.entry(key).or_insert(0);
    *count = count.saturating_add(1);
    *count
}

/// Render JSON deterministically for signature inputs.
pub fn normalize_json(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => value.to_string(),
        Value::Array(values) => {
            let items: Vec<String> = values.iter().map(normalize_json).collect();
            format!("[{}]", items.join(","))
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(key, _)| *key);
            let items: Vec<String> = entries
                .into_iter()
                .map(|(key, value)| format!("{}:{}", json_string(key), normalize_json(value)))
                .collect();
            format!("{{{}}}", items.join(","))
        }
    }
}

fn json_string(value: &str) -> String {
    // SAFETY: serializing a string is infallible.
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""))
}

fn stable_digest(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.len().to_string().as_bytes());
        hasher.update(b":");
        hasher.update(part.as_bytes());
        hasher.update(b";");
    }
    hex_lower(&hasher.finalize())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn default_thresholds_match_epic_values() {
        let config = LoopGuardConfig::default();
        assert_eq!(config.identical_tool_failure_threshold, 3);
        assert_eq!(config.permission_denial_threshold, 2);
        assert_eq!(config.assistant_output_repeat_threshold, 4);
        assert_eq!(config.consecutive_tool_failure_threshold, 6);
    }
    #[test]
    fn json_normalization_sorts_object_keys_but_preserves_arrays() {
        let left = json!({
            "b": [2, 1, {"z": null, "a": true}],
            "a": "text",
            "c": false
        });
        let right = json!({
            "c": false,
            "a": "text",
            "b": [2, 1, {"a": true, "z": null}]
        });
        let different_array_order = json!({
            "a": "text",
            "b": [1, 2, {"a": true, "z": null}],
            "c": false
        });
        assert_eq!(normalize_json(&left), normalize_json(&right));
        assert_ne!(
            normalize_json(&left),
            normalize_json(&different_array_order)
        );
        assert_eq!(
            normalize_json(&left),
            r#"{"a":"text","b":[2,1,{"a":true,"z":null}],"c":false}"#
        );
    }
    #[test]
    fn tool_call_signature_is_stable_for_equivalent_argument_objects() {
        let first = ToolCallSignature::new("shell", &json!({"timeout_ms": 10, "command": "true"}));
        let second = ToolCallSignature::new("shell", &json!({"command": "true", "timeout_ms": 10}));
        let different_tool =
            ToolCallSignature::new("read", &json!({"command": "true", "timeout_ms": 10}));
        assert_eq!(first, second);
        assert_ne!(first, different_tool);
        assert_eq!(
            first.normalized_args,
            r#"{"command":"true","timeout_ms":10}"#
        );
        assert_eq!(first.digest.len(), 64);
    }
    #[test]
    fn assistant_output_signature_includes_text_and_ordered_tool_calls() {
        let shell = ToolCallSignature::new("shell", &json!({"command": "true"}));
        let read = ToolCallSignature::new("read", &json!({"file_path": "Cargo.toml"}));
        let first =
            AssistantOutputSignature::new("I will inspect.", vec![shell.clone(), read.clone()]);
        let repeated =
            AssistantOutputSignature::new("I will inspect.", vec![shell.clone(), read.clone()]);
        let different_text =
            AssistantOutputSignature::new("Different.", vec![shell.clone(), read.clone()]);
        let different_order = AssistantOutputSignature::new("I will inspect.", vec![read, shell]);
        assert_eq!(first, repeated);
        assert_ne!(first, different_text);
        assert_ne!(first, different_order);
        assert_eq!(first.digest.len(), 64);
    }
    #[test]
    fn assistant_output_signature_ignores_runtime_tool_call_ids() {
        let blocks_a = vec![ContentBlock::ToolUse {
            id: "call-random-a".to_string(),
            name: "shell".to_string(),
            input: json!({"command": "true", "timeout_ms": 10}),
        }];
        let blocks_b = vec![ContentBlock::ToolUse {
            id: "call-random-b".to_string(),
            name: "shell".to_string(),
            input: json!({"timeout_ms": 10, "command": "true"}),
        }];
        assert_eq!(
            AssistantOutputSignature::from_content_blocks("Checking", &blocks_a),
            AssistantOutputSignature::from_content_blocks("Checking", &blocks_b)
        );
    }
    #[test]
    fn state_trips_repeated_tool_failure_at_threshold() {
        let mut state = LoopGuardState::default();
        let sig = ToolCallSignature::new("shell", &json!({"command": "false"}));
        assert!(
            state
                .record_tool_failure(sig.clone(), ToolFailureClass::General)
                .is_none()
        );
        assert!(
            state
                .record_tool_failure(sig.clone(), ToolFailureClass::General)
                .is_none()
        );
        let condition = state
            .record_tool_failure(sig.clone(), ToolFailureClass::General)
            .expect("third identical failure trips");
        assert_eq!(condition.kind(), LoopGuardKind::RepeatedToolFailure);
        assert_eq!(condition.observed, 3);
        assert_eq!(condition.threshold, 3);
        assert_eq!(
            condition.reason,
            LoopGuardReason::RepeatedToolFailure { signature: sig }
        );
    }
    #[test]
    fn state_trips_permission_denial_at_stricter_threshold() {
        let mut state = LoopGuardState::default();
        let sig = ToolCallSignature::new("write", &json!({"path": "/etc/passwd"}));
        assert!(
            state
                .record_tool_failure(sig.clone(), ToolFailureClass::PermissionOrSecurityDenial)
                .is_none()
        );
        let condition = state
            .record_tool_failure(sig, ToolFailureClass::PermissionOrSecurityDenial)
            .expect("second identical denial trips");
        assert_eq!(
            condition.kind(),
            LoopGuardKind::RepeatedPermissionOrSecurityDenial
        );
        assert_eq!(condition.observed, 2);
        assert_eq!(condition.threshold, 2);
    }
    #[test]
    fn state_trips_repeated_assistant_output_at_threshold() {
        let mut state = LoopGuardState::default();
        let sig = AssistantOutputSignature::new(
            "same plan",
            vec![ToolCallSignature::new(
                "read",
                &json!({"file_path": "src/lib.rs"}),
            )],
        );
        for _ in 0..3 {
            assert!(state.record_assistant_output(sig.clone()).is_none());
        }
        let condition = state
            .record_assistant_output(sig)
            .expect("fourth identical assistant output trips");
        assert_eq!(condition.kind(), LoopGuardKind::RepeatedAssistantOutput);
        assert_eq!(condition.observed, 4);
        assert_eq!(condition.threshold, 4);
    }
    #[test]
    fn state_tracks_consecutive_failures_across_signatures_and_resets_on_success() {
        let mut state = LoopGuardState::default();
        let repeated_sig = ToolCallSignature::new("shell", &json!({"command": "false"}));
        assert!(
            state
                .record_tool_failure(repeated_sig.clone(), ToolFailureClass::General)
                .is_none()
        );
        assert!(
            state
                .record_tool_failure(repeated_sig.clone(), ToolFailureClass::General)
                .is_none()
        );
        state.record_tool_success();
        assert_eq!(state.consecutive_tool_failures(), 0);
        assert!(
            state
                .record_tool_failure(repeated_sig.clone(), ToolFailureClass::General)
                .is_none()
        );
        assert!(
            state
                .record_tool_failure(repeated_sig, ToolFailureClass::General)
                .is_none()
        );
        state.record_tool_success();
        for idx in 0..5 {
            let sig = ToolCallSignature::new("shell", &json!({"command": format!("false {idx}")}));
            assert!(
                state
                    .record_tool_failure(sig, ToolFailureClass::General)
                    .is_none()
            );
        }
        assert_eq!(state.consecutive_tool_failures(), 5);
        state.record_tool_success();
        assert_eq!(state.consecutive_tool_failures(), 0);
        for idx in 0..5 {
            let sig =
                ToolCallSignature::new("read", &json!({"file_path": format!("missing-{idx}")}));
            assert!(
                state
                    .record_tool_failure(sig, ToolFailureClass::General)
                    .is_none()
            );
        }
        let final_sig = ToolCallSignature::new("read", &json!({"file_path": "missing-final"}));
        let condition = state
            .record_tool_failure(final_sig.clone(), ToolFailureClass::General)
            .expect("sixth consecutive failure trips");
        assert_eq!(condition.kind(), LoopGuardKind::ConsecutiveToolFailures);
        assert_eq!(condition.observed, 6);
        assert_eq!(condition.threshold, 6);
        assert_eq!(
            condition.reason,
            LoopGuardReason::ConsecutiveToolFailures {
                last_signature: final_sig
            }
        );
    }
}
