//! Wire-friendly per-role LLM provider credentials shipped from the
//! coordinator host into the worker Pod via the per-task-run K8s Secret.
//!
//! Phase 7a of the multi-user refactor. The user-chosen design swaps the
//! original Phase 2-audit "every LLM call routes through `invoke_llm` RPC"
//! approach for a Secret-mount: the host resolves all per-role credentials at
//! dispatch time, bincode-serialises them into a sibling
//! [`ResolvedCredentials`], and the [`crate::SessionRuntime`] backend mounts
//! the encoded bytes alongside the [`crate::TaskRunSpec`]. The worker reads
//! the file at start-up and constructs concrete provider clients locally
//! (Phase 7b) — no RPC round-trip per token.
//!
//! ## Why opaque OAuth blobs
//!
//! `djinn-runtime` deliberately only depends on `djinn-core`. The OAuth
//! credential variant carries `config_json: String` — a `serde_json`-encoded
//! `djinn_provider::ProviderConfig` (or a wire mirror thereof) produced by
//! the host crate — so this crate doesn't gain a `djinn-provider` dependency
//! just to type the payload. The worker (which already depends on both
//! `djinn-runtime` and `djinn-provider`) deserialises the blob on read.
//!
//! API-key credentials are wire-typed directly: the `key_name` is the
//! environment-variable shape the catalog expects (`OPENAI_API_KEY`, …) and
//! the `api_key` is the decrypted secret pulled from the credential vault.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::spec::RoleKind;

/// One per-role credential payload, in a shape that survives a bincode
/// round-trip across the host → worker Secret boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SerializableCredential {
    /// Traditional API-key credential. `key_name` is the env-var shape the
    /// provider catalog publishes (e.g. `ANTHROPIC_API_KEY`); `api_key` is
    /// the decrypted value pulled from the credential vault on the host.
    ApiKey { key_name: String, api_key: String },

    /// OAuth-derived provider configuration, serialised as opaque JSON so
    /// `djinn-runtime` doesn't need to import `djinn-provider`. The host
    /// crate produces this blob from `djinn_provider::ProviderConfig` (or a
    /// wire mirror) via `serde_json::to_string`; the worker decodes back
    /// before constructing a concrete `LlmProvider`.
    OAuthConfig { config_json: String },
}

/// Bundle of per-role credentials handed to [`crate::SessionRuntime::prepare`]
/// at task-run dispatch.
///
/// Empty `per_role` is a valid wire value — flows that don't execute an LLM
/// stage (e.g. a future repair-only flow) simply pass `default()`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedCredentials {
    /// One credential entry per [`RoleKind`] active in the
    /// [`crate::SupervisorFlow`]. Roles missing from the map are treated as
    /// "no credential resolved" by downstream consumers — the worker decides
    /// whether that is fatal at provider-construction time.
    pub per_role: HashMap<RoleKind, SerializableCredential>,
}

impl ResolvedCredentials {
    /// Convenience constructor matching `HashMap::new()` ergonomics.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a per-role credential, returning the previous entry (if any).
    pub fn insert(
        &mut self,
        role: RoleKind,
        credential: SerializableCredential,
    ) -> Option<SerializableCredential> {
        self.per_role.insert(role, credential)
    }

    /// All [`RoleKind`]s that have a credential resolved — useful for the
    /// worker startup log line.
    pub fn roles(&self) -> impl Iterator<Item = &RoleKind> {
        self.per_role.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_credentials_bincode_roundtrip() {
        let mut creds = ResolvedCredentials::new();
        creds.insert(
            RoleKind::Worker,
            SerializableCredential::ApiKey {
                key_name: "ANTHROPIC_API_KEY".into(),
                api_key: "sk-ant-fake".into(),
            },
        );
        creds.insert(
            RoleKind::Planner,
            SerializableCredential::OAuthConfig {
                // Realistic shape: a JSON blob the worker will later decode
                // back into a `djinn_provider::ProviderConfig` mirror.
                config_json: r#"{"base_url":"https://api.example","model_id":"x"}"#.into(),
            },
        );

        let bytes = bincode::serialize(&creds).expect("serialize");
        let back: ResolvedCredentials = bincode::deserialize(&bytes).expect("deserialize");

        assert_eq!(back.per_role.len(), 2);
        assert_eq!(back, creds);

        match back.per_role.get(&RoleKind::Worker).expect("worker entry") {
            SerializableCredential::ApiKey { key_name, api_key } => {
                assert_eq!(key_name, "ANTHROPIC_API_KEY");
                assert_eq!(api_key, "sk-ant-fake");
            }
            other => panic!("expected ApiKey, got {other:?}"),
        }
        match back.per_role.get(&RoleKind::Planner).expect("planner entry") {
            SerializableCredential::OAuthConfig { config_json } => {
                assert!(config_json.contains("api.example"));
            }
            other => panic!("expected OAuthConfig, got {other:?}"),
        }
    }

    #[test]
    fn empty_resolved_credentials_roundtrips() {
        let creds = ResolvedCredentials::default();
        let bytes = bincode::serialize(&creds).expect("serialize");
        let back: ResolvedCredentials = bincode::deserialize(&bytes).expect("deserialize");
        assert!(back.per_role.is_empty());
    }
}
