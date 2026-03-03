use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::models::provider::{Model, Pricing, Provider};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_snapshot_parses() {
        let catalog = CatalogService::new();
        let providers = catalog.list_providers();
        assert!(!providers.is_empty(), "embedded snapshot should have providers");
    }

    #[test]
    fn list_models_for_known_provider() {
        let catalog = CatalogService::new();
        let models = catalog.list_models("anthropic");
        assert!(!models.is_empty(), "anthropic should have models in snapshot");
        for m in &models {
            assert_eq!(m.provider_id, "anthropic");
        }
    }

    #[test]
    fn find_model_by_full_id() {
        let catalog = CatalogService::new();
        // Use any model that should be in the snapshot.
        let providers = catalog.list_providers();
        let provider = providers.iter().find(|p| !catalog.list_models(&p.id).is_empty());
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
}
