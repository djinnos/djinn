use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::models::provider::{Model, Pricing, Provider};
use goose::providers::base::{ProviderMetadata, ProviderType};

const CATALOG_URL: &str = "https://models.dev/api.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

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

struct CatalogData {
    providers: Vec<Provider>,
    models_idx: HashMap<String, Vec<Model>>,
    fetched_at: Option<Instant>,
}

impl Default for CatalogData {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            models_idx: HashMap::new(),
            fetched_at: None,
        }
    }
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
                let mut data = self.inner.write().unwrap();
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
                let mut data = self.inner.write().unwrap();
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
        self.inner.read().unwrap().providers.clone()
    }

    pub fn list_models(&self, provider_id: &str) -> Vec<Model> {
        self.inner
            .read()
            .unwrap()
            .models_idx
            .get(provider_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Find a model by its full `"providerID/modelID"` identifier.
    /// Returns `None` if not found or if the ID is not in the expected format.
    pub fn find_model(&self, full_model_id: &str) -> Option<Model> {
        let (provider_id, model_id) = full_model_id.split_once('/')?;
        self.list_models(provider_id)
            .into_iter()
            .find(|m| m.id == model_id)
    }

    // ── Write accessors ───────────────────────────────────────────────────────

    /// Inject synthetic catalog entries for Goose-registered providers that have no
    /// corresponding models.dev entry.  This makes providers like `chatgpt_codex` and
    /// `gcp_vertex_ai` visible in `provider_catalog` without requiring them to exist
    /// in the upstream models.dev JSON.
    ///
    /// Model lists are sourced from models.dev when a mapping exists (see
    /// [`MODEL_SOURCE_MAP`]), falling back to Goose's hardcoded `known_models`.
    pub fn inject_goose_providers(&self, entries: &[(ProviderMetadata, ProviderType)]) {
        let mut data = self.inner.write().unwrap();
        let existing_ids: HashSet<String> = data.providers.iter().map(|p| p.id.clone()).collect();

        for (meta, _) in entries {
            if existing_ids.contains(&meta.name) {
                continue;
            }

            let env_vars: Vec<String> = meta
                .config_keys
                .iter()
                .filter(|k| k.required)
                .map(|k| k.name.clone())
                .collect();

            let provider = Provider {
                id: meta.name.clone(),
                name: meta.display_name.clone(),
                npm: String::new(),
                env_vars,
                base_url: String::new(),
                docs_url: meta.model_doc_link.clone(),
                is_openai_compatible: false, // filtered via goose_ids instead
            };
            data.providers.push(provider);

            // Try to source models from models.dev via the mapping table.
            let models = self
                .models_from_catalog_source(&data, &meta.name)
                .unwrap_or_else(|| models_from_goose_metadata(meta));

            if !models.is_empty() {
                data.models_idx.insert(meta.name.clone(), models);
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
        goose_provider_id: &str,
    ) -> Option<Vec<Model>> {
        let (_, source_id, prefix) = MODEL_SOURCE_MAP
            .iter()
            .find(|(goose_id, _, _)| *goose_id == goose_provider_id)?;

        let source_models = data.models_idx.get(*source_id)?;
        let models: Vec<Model> = source_models
            .iter()
            .filter(|m| match prefix {
                Some(pfx) => m.id.contains(pfx),
                None => true,
            })
            .map(|m| Model {
                provider_id: goose_provider_id.to_string(),
                ..m.clone()
            })
            .collect();

        if models.is_empty() {
            None
        } else {
            Some(models)
        }
    }

    /// Add or replace a custom provider and its seed models in the in-memory catalog.
    /// Persisting to DB is the caller's responsibility.
    pub fn add_custom_provider(&self, provider: Provider, seed_models: Vec<Model>) {
        let mut data = self.inner.write().unwrap();
        data.providers.retain(|p| p.id != provider.id);
        data.models_idx.remove(&provider.id);

        data.providers.push(provider.clone());
        data.providers.sort_by(|a, b| a.id.cmp(&b.id));

        if !seed_models.is_empty() {
            data.models_idx.insert(provider.id, seed_models);
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
            .map(|rm| Model {
                id: rm.id,
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

// ── Goose → models.dev model source mapping ──────────────────────────────────
//
// Maps Goose-only provider IDs to a models.dev provider whose model list should
// be used instead of Goose's hardcoded `known_models`.  The optional filter
// prefix narrows the source list to relevant models.
//
// (goose_provider_id, models_dev_provider_id, optional_model_name_filter)
const MODEL_SOURCE_MAP: &[(&str, &str, Option<&str>)] = &[
    ("chatgpt_codex", "openai", Some("codex")),
    ("gcp_vertex_ai", "google-vertex", None),
    ("aws_bedrock", "amazon-bedrock", None),
    ("azure_openai", "azure", None),
    ("codex", "openai", Some("codex")),
    ("claude-code", "anthropic", None),
    ("gemini-cli", "google", None),
];

/// Build a model list from Goose's `ProviderMetadata.known_models` (fallback).
fn models_from_goose_metadata(meta: &ProviderMetadata) -> Vec<Model> {
    meta.known_models
        .iter()
        .map(|info| Model {
            id: info.name.clone(),
            provider_id: meta.name.clone(),
            name: info.name.clone(),
            tool_call: true,
            reasoning: false,
            attachment: false,
            context_window: info.context_limit as i64,
            output_limit: 0,
            pricing: Pricing {
                input_per_million: info.input_token_cost.unwrap_or(0.0) * 1_000_000.0,
                output_per_million: info.output_token_cost.unwrap_or(0.0) * 1_000_000.0,
                ..Pricing::default()
            },
        })
        .collect()
}

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
    fn inject_goose_providers_adds_missing_entries() {
        use goose::providers::base::ConfigKey;

        let catalog = CatalogService::new();
        let initial_count = catalog.list_providers().len();

        // Simulate a Goose-only provider not in models.dev.
        let meta = ProviderMetadata::new(
            "test_oauth_provider",
            "Test OAuth",
            "An OAuth-only test provider",
            "test-model-v1",
            vec!["test-model-v1", "test-model-v2"],
            "https://example.com/docs",
            vec![ConfigKey::new_oauth(
                "TEST_OAUTH_TOKEN",
                true,
                true,
                None,
                false,
            )],
        );
        let entries = vec![(meta, ProviderType::Preferred)];
        catalog.inject_goose_providers(&entries);

        let providers = catalog.list_providers();
        assert_eq!(providers.len(), initial_count + 1);

        let injected = providers
            .iter()
            .find(|p| p.id == "test_oauth_provider")
            .expect("injected provider should exist");
        assert_eq!(injected.name, "Test OAuth");
        assert!(!injected.is_openai_compatible);

        let models = catalog.list_models("test_oauth_provider");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].provider_id, "test_oauth_provider");
    }

    #[test]
    fn inject_goose_providers_skips_existing() {
        let catalog = CatalogService::new();
        let initial_count = catalog.list_providers().len();

        // "anthropic" is already in the snapshot — should not be duplicated.
        let meta = ProviderMetadata::new(
            "anthropic",
            "Anthropic (dupe)",
            "",
            "claude-3",
            vec![],
            "",
            vec![],
        );
        let entries = vec![(meta, ProviderType::Preferred)];
        catalog.inject_goose_providers(&entries);

        assert_eq!(catalog.list_providers().len(), initial_count);
    }
}
