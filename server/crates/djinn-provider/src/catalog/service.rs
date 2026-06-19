use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde::Deserialize;

use djinn_core::models::{Credential, Model, Pricing, Provider};

use crate::catalog::builtin::BuiltinProvider;

const CATALOG_URL: &str = "https://models.dev/api.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

fn has_nonzero_pricing(pricing: &Pricing) -> bool {
    pricing.input_per_million != 0.0
        || pricing.output_per_million != 0.0
        || pricing.cache_read_per_million != 0.0
        || pricing.cache_write_per_million != 0.0
}

/// Build-time embedded snapshot of models.dev/api.json.
/// Used when no live data is available.
static EMBEDDED_SNAPSHOT: &[u8] = include_bytes!("snapshot.json");

// ── Raw JSON structures from models.dev ──────────────────────────────────────

#[derive(Deserialize)]
struct RawProvider {
    #[serde(default)]
    id: String,
    #[serde(default)]
    npm: String,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    api: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    doc: String,
    #[serde(default)]
    models: HashMap<String, RawModel>,
}

#[derive(Deserialize)]
struct RawModel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    tool_call: bool,
    #[serde(default)]
    reasoning: bool,
    #[serde(default)]
    attachment: bool,
    #[serde(default)]
    cost: RawCost,
    #[serde(default)]
    limit: RawLimit,
}

#[derive(Deserialize, Default)]
struct RawCost {
    #[serde(default)]
    input: f64,
    #[serde(default)]
    output: f64,
    #[serde(default)]
    cache_read: f64,
    #[serde(default)]
    cache_write: f64,
}

#[derive(Deserialize, Default)]
struct RawLimit {
    #[serde(default)]
    context: i64,
    #[serde(default)]
    output: i64,
}

// ── Catalog internals ─────────────────────────────────────────────────────────

#[derive(Default)]
struct CatalogData {
    providers: Vec<Provider>,
    models_idx: HashMap<String, Vec<Model>>,
    fetched_at: Option<Instant>,
}

/// Fetches, caches, and serves LLM provider and model data from models.dev.
///
/// Resilience tiers (in order):
/// 1. Fresh fetch from models.dev (within TTL)
/// 2. Stale in-memory cache (previous successful fetch)
/// 3. Embedded snapshot (build-time bundled JSON)
///
/// All read methods are safe for concurrent use without blocking.
#[derive(Clone)]
pub struct CatalogService {
    inner: Arc<RwLock<CatalogData>>,
}

impl CatalogService {
    /// Create a new catalog service seeded from the embedded snapshot.
    pub fn new() -> Self {
        let svc = Self {
            inner: Arc::new(RwLock::new(CatalogData::default())),
        };
        svc.seed_from_embedded();
        svc
    }

    fn seed_from_embedded(&self) {
        match serde_json::from_slice::<HashMap<String, RawProvider>>(EMBEDDED_SNAPSHOT) {
            Ok(raw) => {
                let (providers, models_idx) = normalize(raw);
                let mut data = self.inner.write();
                data.providers = providers;
                data.models_idx = models_idx;
                // Do NOT set fetched_at — embedded data is stale by design.
            }
            Err(e) => {
                tracing::error!(error = %e, "embedded provider catalog snapshot parse error");
            }
        }
    }

    /// Attempt a live fetch from models.dev.  Replaces cached data on success;
    /// preserves embedded/stale data on failure.
    pub async fn refresh(&self) {
        match self.fetch_remote().await {
            Ok(raw) => {
                let (providers, models_idx) = normalize(raw);
                let mut data = self.inner.write();
                data.providers = providers;
                data.models_idx = models_idx;
                data.fetched_at = Some(Instant::now());
                tracing::info!("provider catalog refreshed from models.dev");
            }
            Err(e) => {
                tracing::warn!(error = %e, "catalog refresh failed — using cached/embedded data");
            }
        }
    }

    async fn fetch_remote(&self) -> Result<HashMap<String, RawProvider>, String> {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;

        let resp = client
            .get(CATALOG_URL)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("models.dev returned HTTP {}", resp.status()));
        }

        resp.json::<HashMap<String, RawProvider>>()
            .await
            .map_err(|e| e.to_string())
    }

    // ── Read accessors ────────────────────────────────────────────────────────

    pub fn list_providers(&self) -> Vec<Provider> {
        self.inner.read().providers.clone()
    }

    pub fn list_models(&self, provider_id: &str) -> Vec<Model> {
        self.inner
            .read()
            .models_idx
            .get(provider_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Find a model by its full `"providerID/modelID"` identifier.
    /// Returns `None` if not found or if the ID is not in the expected format.
    pub fn find_model(&self, full_model_id: &str) -> Option<Model> {
        let (provider_id, model_id) = full_model_id.split_once('/')?;
        self.list_models(provider_id).into_iter().find(|m| {
            let bare = m.id.rsplit('/').next().unwrap_or(&m.id);
            bare == model_id || m.id == model_id || m.id == full_model_id
        })
    }

    /// Collect per-million-token pricing for every priced model in the catalog,
    /// keyed by the full `"providerID/modelID"` identifier that sessions store
    /// in `model_id`.
    ///
    /// Models whose catalog entry carries `Pricing::default()` (all rates zero)
    /// are treated as unpriced and intentionally omitted.  Custom-provider seed
    /// models use that default because no price is known; including them would
    /// backfill `cost_usd = 0` and incorrectly encode "free" instead of
    /// "unknown".
    ///
    /// Used by the startup pricing-snapshot backfill to obtain a single
    /// pass-through map from `djinn-provider` → `djinn-db` without coupling
    /// the DB crate to the provider crate.
    pub fn pricing_for_all_models(&self) -> HashMap<String, Pricing> {
        let mut map = HashMap::new();
        for provider in self.list_providers() {
            for model in self.list_models(&provider.id) {
                // Normalise to "providerID/modelID" so it matches the
                // `sessions.model_id` value produced at dispatch time.
                let full_id = if model.id.contains('/') {
                    model.id.clone()
                } else {
                    format!("{}/{}", provider.id, model.id)
                };
                if has_nonzero_pricing(&model.pricing) {
                    map.insert(full_id, model.pricing);
                }
            }
        }
        map
    }

    // ── Write accessors ───────────────────────────────────────────────────────

    /// Inject synthetic catalog entries for built-in providers that have no
    /// corresponding models.dev entry.  This makes providers like `chatgpt_codex` and
    /// `gcp_vertex_ai` visible in `provider_catalog` without requiring them to exist
    /// in the upstream models.dev JSON.
    ///
    /// Model lists are sourced from models.dev when a mapping exists (see
    /// [`MODEL_SOURCE_MAP`]).
    pub fn inject_builtin_providers(&self, entries: &[BuiltinProvider]) {
        let mut data = self.inner.write();
        let existing_ids: HashSet<String> = data.providers.iter().map(|p| p.id.clone()).collect();

        for bp in entries {
            if existing_ids.contains(bp.id) {
                continue;
            }

            let provider = Provider {
                id: bp.id.to_string(),
                name: bp.display_name.to_string(),
                npm: String::new(),
                env_vars: bp.required_env_vars.iter().map(|s| s.to_string()).collect(),
                base_url: String::new(),
                docs_url: bp.docs_url.to_string(),
                is_openai_compatible: false, // filtered via builtin_ids instead
            };
            data.providers.push(provider);

            // Try to source models from models.dev via the mapping table.
            if let Some(models) = self.models_from_catalog_source(&data, bp.id) {
                data.models_idx.insert(bp.id.to_string(), models);
            }
        }

        data.providers.sort_by(|a, b| a.id.cmp(&b.id));
    }

    /// Pull models from a mapped models.dev provider, re-tagged with the target
    /// provider ID and filtered by the optional prefix.  Returns `None` when no
    /// mapping exists or the source provider has no models.
    fn models_from_catalog_source(
        &self,
        data: &CatalogData,
        builtin_provider_id: &str,
    ) -> Option<Vec<Model>> {
        let (_, source_id, prefix) = MODEL_SOURCE_MAP
            .iter()
            .find(|(builtin_id, _, _)| *builtin_id == builtin_provider_id)?;

        let source_models = data.models_idx.get(*source_id)?;
        let models: Vec<Model> = source_models
            .iter()
            .filter(|m| match prefix {
                Some(pfx) => m.id.contains(pfx),
                None => true,
            })
            .map(|m| Model {
                provider_id: builtin_provider_id.to_string(),
                ..m.clone()
            })
            .collect();

        if models.is_empty() {
            None
        } else {
            Some(models)
        }
    }

    /// Compute the set of provider IDs that have valid credentials,
    /// combining API-key credentials from the vault with OAuth token checks
    /// and merged-child propagation.
    ///
    /// This is the single source of truth for "is provider X connected?".
    pub fn connected_provider_ids(&self, vault_credentials: &[Credential]) -> HashSet<String> {
        use super::builtin;

        // 1. API-key credentials from the vault.
        let mut connected: HashSet<String> = vault_credentials
            .iter()
            .map(|c| c.provider_id.clone())
            .collect();

        let credential_key_names: HashSet<String> = vault_credentials
            .iter()
            .map(|c| c.key_name.clone())
            .collect();

        // 2. OAuth-connected providers (own keys + merged children).
        for provider in self.list_providers() {
            let oauth_keys = builtin::all_oauth_keys_for_provider(&provider.id);
            if !oauth_keys.is_empty()
                && builtin::is_oauth_key_present(&oauth_keys, &credential_key_names)
            {
                connected.insert(provider.id.clone());
            }
        }

        // 3. Merged children propagate connectivity to their parent.
        // E.g. chatgpt_codex (connected via OAuth) → openai is also connected.
        for bp in builtin::BUILTIN_PROVIDERS {
            if let Some(parent_id) = bp.merge_into {
                let child_oauth: Vec<String> =
                    bp.oauth_keys.iter().map(|k| k.to_string()).collect();
                let child_connected = connected.contains(bp.id)
                    || (!child_oauth.is_empty()
                        && builtin::is_oauth_key_present(&child_oauth, &credential_key_names));
                if child_connected {
                    connected.insert(parent_id.to_string());
                }
            }
        }

        connected
    }

    /// Remove a custom provider and its models from the in-memory catalog.
    /// Persisting to DB is the caller's responsibility.
    pub fn remove_custom_provider(&self, provider_id: &str) {
        let mut data = self.inner.write();
        data.providers.retain(|p| p.id != provider_id);
        data.models_idx.remove(provider_id);
    }

    /// Add or replace a custom provider and its seed models in the in-memory catalog.
    /// Persisting to DB is the caller's responsibility.
    pub fn add_custom_provider(&self, provider: Provider, seed_models: Vec<Model>) {
        let mut data = self.inner.write();
        data.providers.retain(|p| p.id != provider.id);
        data.models_idx.remove(&provider.id);

        data.providers.push(provider.clone());
        data.providers.sort_by(|a, b| a.id.cmp(&b.id));

        if !seed_models.is_empty() {
            // Strip provider prefix from model IDs (some sources store the full
            // "provider/model" form; internal IDs should be the bare model name).
            let prefix = format!("{}/", provider.id);
            let normalized: Vec<Model> = seed_models
                .into_iter()
                .map(|mut m| {
                    if let Some(bare) = m.id.strip_prefix(&prefix) {
                        m.id = bare.to_string();
                    }
                    m
                })
                .collect();
            data.models_idx.insert(provider.id, normalized);
        }
    }
}

impl Default for CatalogService {
    fn default() -> Self {
        Self::new()
    }
}

// ── Normalization ─────────────────────────────────────────────────────────────

fn normalize(raw: HashMap<String, RawProvider>) -> (Vec<Provider>, HashMap<String, Vec<Model>>) {
    let mut providers = Vec::with_capacity(raw.len());
    let mut models_idx: HashMap<String, Vec<Model>> = HashMap::with_capacity(raw.len());

    for (_, rp) in raw {
        if rp.id.is_empty() {
            continue;
        }
        let provider = Provider {
            id: rp.id.clone(),
            name: rp.name,
            npm: rp.npm.clone(),
            env_vars: rp.env,
            base_url: rp.api,
            docs_url: rp.doc,
            is_openai_compatible: is_openai_compatible(&rp.npm),
        };

        let mut models: Vec<Model> = rp
            .models
            .into_values()
            .filter(|rm| !rm.id.is_empty())
            .map(|rm| {
                // Some providers (e.g. synthetic) include the provider prefix in
                // model IDs ("synthetic/GLM-4.7").  Strip it so internal IDs are
                // always the bare model name.
                let bare_id = rm
                    .id
                    .strip_prefix(&format!("{}/", rp.id))
                    .map(|s| s.to_string())
                    .unwrap_or(rm.id);
                Model {
                    id: bare_id,
                    provider_id: rp.id.clone(),
                    name: rm.name,
                    tool_call: rm.tool_call,
                    reasoning: rm.reasoning,
                    attachment: rm.attachment,
                    context_window: rm.limit.context,
                    output_limit: rm.limit.output,
                    pricing: Pricing {
                        input_per_million: rm.cost.input,
                        output_per_million: rm.cost.output,
                        cache_read_per_million: rm.cost.cache_read,
                        cache_write_per_million: rm.cost.cache_write,
                    },
                }
            })
            .collect();

        models.sort_by(|a, b| a.id.cmp(&b.id));
        if !models.is_empty() {
            models_idx.insert(rp.id.clone(), models);
        }
        providers.push(provider);
    }

    providers.sort_by(|a, b| a.id.cmp(&b.id));
    (providers, models_idx)
}

fn is_openai_compatible(npm: &str) -> bool {
    npm.contains("openai-compatible") || npm == "@ai-sdk/openai"
}

// ── Built-in → models.dev model source mapping ───────────────────────────────
//
// Maps built-in provider IDs to a models.dev provider whose model list should
// be used.  The optional filter prefix narrows the source list to relevant
// models.
//
// (builtin_provider_id, models_dev_provider_id, optional_model_name_filter)
const MODEL_SOURCE_MAP: &[(&str, &str, Option<&str>)] = &[
    ("chatgpt_codex", "openai", Some("codex")),
    ("gcp_vertex_ai", "google-vertex", None),
    ("aws_bedrock", "amazon-bedrock", None),
    ("azure_openai", "azure", None),
    ("codex", "openai", Some("codex")),
    ("claude-code", "anthropic", None),
    ("gemini-cli", "google", None),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_snapshot_parses() {
        let catalog = CatalogService::new();
        let providers = catalog.list_providers();
        assert!(
            !providers.is_empty(),
            "embedded snapshot should have providers"
        );
    }

    #[test]
    fn connected_includes_openai_via_chatgpt_codex_merge() {
        let catalog = CatalogService::new();
        let cred = |provider_id: &str, key_name: &str, owner: Option<&str>| {
            djinn_core::models::Credential {
                id: provider_id.into(),
                provider_id: provider_id.into(),
                key_name: key_name.into(),
                owner_user_id: owner.map(str::to_string),
                created_at: String::new(),
                updated_at: String::new(),
            }
        };
        let creds = vec![
            cred("chatgpt_codex", "__OAUTH_CHATGPT_CODEX", Some("u1")),
            cred("fireworks-ai", "FIREWORKS_API_KEY", None),
        ];
        let connected = catalog.connected_provider_ids(&creds);
        assert!(connected.contains("chatgpt_codex"), "got {connected:?}");
        assert!(connected.contains("fireworks-ai"), "got {connected:?}");
        assert!(
            connected.contains("openai"),
            "chatgpt_codex must merge → openai connected; got {connected:?}"
        );
    }

    #[test]
    fn list_models_for_known_provider() {
        let catalog = CatalogService::new();
        let models = catalog.list_models("anthropic");
        assert!(
            !models.is_empty(),
            "anthropic should have models in snapshot"
        );
        for m in &models {
            assert_eq!(m.provider_id, "anthropic");
        }
    }

    #[test]
    fn pricing_for_all_models_omits_unpriced_seed_models() {
        let catalog = CatalogService::new();
        let provider = Provider {
            id: "custom-unpriced".to_string(),
            name: "Custom Unpriced".to_string(),
            npm: String::new(),
            env_vars: vec!["CUSTOM_API_KEY".to_string()],
            base_url: "https://example.invalid/v1".to_string(),
            docs_url: String::new(),
            is_openai_compatible: true,
        };
        catalog.add_custom_provider(
            provider,
            vec![Model {
                id: "seed-model".to_string(),
                provider_id: "custom-unpriced".to_string(),
                name: "Seed Model".to_string(),
                tool_call: false,
                reasoning: false,
                attachment: false,
                context_window: 0,
                output_limit: 0,
                pricing: Pricing::default(),
            }],
        );

        let pricing = catalog.pricing_for_all_models();
        assert!(
            !pricing.contains_key("custom-unpriced/seed-model"),
            "all-zero/default pricing means unknown, not free, and must not be backfilled"
        );
    }

    #[test]
    fn find_model_by_full_id() {
        let catalog = CatalogService::new();
        // Use any model that should be in the snapshot.
        let providers = catalog.list_providers();
        let provider = providers
            .iter()
            .find(|p| !catalog.list_models(&p.id).is_empty());
        if let Some(p) = provider {
            let models = catalog.list_models(&p.id);
            let m = &models[0];
            let full_id = format!("{}/{}", p.id, m.id);
            let found = catalog.find_model(&full_id);
            assert!(found.is_some(), "should find model by full ID {full_id}");
        }
    }

    /// Xiaomi MiMo Token Plan ships with dotted model ids (`mimo-v2.5-pro`).
    /// These must round-trip as `xiaomi-token-plan-sgp/mimo-v2.5-pro` without the
    /// catalog split logic mangling the dot (cf. the Fireworks multi-segment 404
    /// bug). `xiaomi-token-plan-sgp` is models.dev-native (its models arrive via
    /// the live catalog refresh, not the embedded snapshot), so the dotted model
    /// list is seeded here to exercise the split logic in isolation.
    #[test]
    fn xiaomi_token_plan_sgp_dotted_model_id_round_trips() {
        let catalog = CatalogService::new();
        let provider = Provider {
            id: "xiaomi-token-plan-sgp".to_string(),
            name: "Xiaomi MiMo Token Plan (SGP)".to_string(),
            npm: "@ai-sdk/openai-compatible".to_string(),
            env_vars: vec!["XIAOMI_API_KEY".to_string()],
            base_url: "https://token-plan-sgp.xiaomimimo.com/v1".to_string(),
            docs_url: "https://platform.xiaomimimo.com".to_string(),
            is_openai_compatible: true,
        };
        let seed = |id: &str, name: &str| Model {
            id: id.to_string(),
            provider_id: "xiaomi-token-plan-sgp".to_string(),
            name: name.to_string(),
            tool_call: true,
            reasoning: true,
            attachment: false,
            context_window: 1_000_000,
            output_limit: 64_000,
            pricing: Pricing::default(),
        };
        catalog.add_custom_provider(
            provider,
            vec![
                seed("mimo-v2.5-pro", "MiMo-V2.5-Pro"),
                seed("mimo-v2.5", "MiMo-V2.5"),
            ],
        );

        let models = catalog.list_models("xiaomi-token-plan-sgp");
        assert_eq!(
            models.len(),
            2,
            "xiaomi-token-plan-sgp should expose mimo-v2.5-pro + mimo-v2.5; got {models:?}"
        );
        for full in [
            "xiaomi-token-plan-sgp/mimo-v2.5-pro",
            "xiaomi-token-plan-sgp/mimo-v2.5",
        ] {
            let found = catalog
                .find_model(full)
                .unwrap_or_else(|| panic!("should resolve dotted full id {full}"));
            assert_eq!(found.provider_id, "xiaomi-token-plan-sgp");
            // The dot in `v2.5` must survive intact.
            assert_eq!(format!("xiaomi-token-plan-sgp/{}", found.id), full);
        }
    }

    #[test]
    fn find_model_returns_none_for_bad_id() {
        let catalog = CatalogService::new();
        assert!(catalog.find_model("no-slash").is_none());
        assert!(catalog.find_model("unknown/unknown").is_none());
    }

    #[test]
    fn add_custom_provider_merges_into_catalog() {
        let catalog = CatalogService::new();
        let initial_count = catalog.list_providers().len();

        let provider = Provider {
            id: "my-custom".to_string(),
            name: "My Custom LLM".to_string(),
            npm: String::new(),
            env_vars: vec!["MY_CUSTOM_API_KEY".to_string()],
            base_url: "https://api.my-custom.com/v1".to_string(),
            docs_url: String::new(),
            is_openai_compatible: true,
        };
        catalog.add_custom_provider(provider, vec![]);

        let providers = catalog.list_providers();
        assert_eq!(providers.len(), initial_count + 1);
        assert!(providers.iter().any(|p| p.id == "my-custom"));
    }

    #[test]
    fn inject_builtin_providers_adds_missing_entries() {
        use crate::catalog::builtin::BuiltinProvider;

        let catalog = CatalogService::new();
        let initial_count = catalog.list_providers().len();

        let entries = &[BuiltinProvider {
            id: "test_builtin",
            display_name: "Test Builtin",
            required_env_vars: &["TEST_API_KEY"],
            oauth_keys: &[],
            docs_url: "https://example.com/docs",
            merge_into: None,
            auth_only: false,
            format_rule: crate::catalog::builtin::DEFAULT_FORMAT_RULE,
            auth_shape: crate::catalog::builtin::DEFAULT_AUTH_SHAPE,
            streaming: true,
            max_tokens_default: None,
        }];
        catalog.inject_builtin_providers(entries);

        let providers = catalog.list_providers();
        assert_eq!(providers.len(), initial_count + 1);

        let injected = providers
            .iter()
            .find(|p| p.id == "test_builtin")
            .expect("injected provider should exist");
        assert_eq!(injected.name, "Test Builtin");
        assert!(!injected.is_openai_compatible);
    }

    #[test]
    fn inject_builtin_providers_skips_existing() {
        use crate::catalog::builtin::BuiltinProvider;

        let catalog = CatalogService::new();
        let initial_count = catalog.list_providers().len();

        // "anthropic" is already in the snapshot — should not be duplicated.
        let entries = &[BuiltinProvider {
            id: "anthropic",
            display_name: "Anthropic (dupe)",
            required_env_vars: &[],
            oauth_keys: &[],
            docs_url: "",
            merge_into: None,
            auth_only: false,
            format_rule: crate::catalog::builtin::DEFAULT_FORMAT_RULE,
            auth_shape: crate::catalog::builtin::DEFAULT_AUTH_SHAPE,
            streaming: true,
            max_tokens_default: None,
        }];
        catalog.inject_builtin_providers(entries);

        assert_eq!(catalog.list_providers().len(), initial_count);
    }
}
