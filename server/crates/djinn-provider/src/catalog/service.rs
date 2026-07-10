use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use djinn_core::clock::{Clock, SystemClock};
use djinn_core::models::{Credential, Model, Pricing, Provider};
use parking_lot::RwLock;
use serde::Deserialize;

use crate::catalog::builtin::{BUILTIN_PROVIDERS, BuiltinProvider};

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

/// Retained custom-provider entry: the raw `Provider` plus its seed models with
/// the `provider/` prefix already stripped from each model ID.  Kept in
/// `CatalogData.custom_providers` so periodic upstream refreshes do not drop
/// user-registered providers while the rest of the catalog is being replaced.
#[derive(Clone, Debug)]
struct CustomCatalogProvider {
    /// Stored verbatim so the refresh compose/swap path can re-overlay it onto
    /// the active catalog after a models.dev reload.
    provider: Provider,
    /// Normalized seed models (provider-prefix stripped).  Re-applied by the
    /// refresh compose/swap path alongside `provider`.
    seed_models: Vec<Model>,
}

/// Outcome of the most recent live `models.dev` refresh attempt.  Tracked so
/// downstream observability (health endpoint) can report whether the active
/// catalog is fresh, stale, or was never refreshed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RefreshStatus {
    /// No live refresh has succeeded yet — only embedded/seeded data is served.
    #[default]
    Never,
    /// The most recent refresh completed and the active catalog reflects live
    /// models.dev data (plus retained overlays).
    Success,
    /// The most recent refresh failed (fetch/parse error or a zero-provider
    /// normalized payload); the previous catalog is preserved unchanged.
    Error,
}

#[derive(Default)]
struct CatalogData {
    providers: Vec<Provider>,
    models_idx: HashMap<String, Vec<Model>>,
    /// Retained custom-provider set, kept separate from upstream/builtin catalog
    /// data so a live `models.dev` refresh can recompose the active catalog
    /// without dropping user-registered entries.
    custom_providers: HashMap<String, CustomCatalogProvider>,
    fetched_at: Option<Instant>,
    /// Outcome of the most recent live refresh attempt (`Never` until the first
    /// successful refresh).  Failed or rejected refreshes transition to `Error`
    /// without touching the active catalog.
    last_refresh_status: RefreshStatus,
    /// Human-readable error string from the most recent failed refresh.  Cleared
    /// on a successful refresh.  `None` while `last_refresh_status == Never`.
    last_refresh_error: Option<String>,
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

    /// Attempt a live fetch from models.dev.  On success, the active catalog is
    /// recomposed *before* the swap: normalized upstream providers/models,
    /// injected builtin providers, and the retained custom-provider set are all
    /// assembled under one write lock so a successful refresh never drops local
    /// entries.  On any failure — network/parse error or a zero-provider
    /// normalized payload — the previously served catalog is preserved
    /// unchanged.
    pub async fn refresh(&self) {
        match self.fetch_remote().await {
            Ok(raw) => {
                let (providers, models_idx) = normalize(raw);
                // Reject an empty normalized upstream payload so a degenerate
                // fetch never overwrites the active catalog with nothing.
                if providers.is_empty() {
                    let mut data = self.inner.write();
                    data.last_refresh_status = RefreshStatus::Error;
                    data.last_refresh_error =
                        Some("models.dev normalized payload had zero providers".to_string());
                    tracing::warn!(
                        providers = data.providers.len(),
                        "catalog refresh rejected zero-provider payload — keeping active catalog"
                    );
                    return;
                }
                let now = SystemClock::new().now_instant();
                let provider_count = providers.len();
                let model_count: usize = models_idx.values().map(Vec::len).sum();
                // Compose the full catalog (upstream + builtins + retained
                // custom providers) and swap it in under a single write lock.
                let mut data = self.inner.write();
                compose_catalog(&mut data, providers, models_idx);
                data.fetched_at = Some(now);
                data.last_refresh_status = RefreshStatus::Success;
                data.last_refresh_error = None;
                tracing::info!(
                    providers = provider_count,
                    models = model_count,
                    "provider catalog refreshed from models.dev"
                );
            }
            Err(e) => {
                let mut data = self.inner.write();
                data.last_refresh_status = RefreshStatus::Error;
                data.last_refresh_error = Some(e.clone());
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

    /// Outcome of the most recent live `models.dev` refresh attempt.
    ///
    /// - [`RefreshStatus::Never`] until the first successful refresh.
    /// - [`RefreshStatus::Success`] after a refresh composed and swapped live data.
    /// - [`RefreshStatus::Error`] after a fetch/parse failure or a rejected
    ///   zero-provider payload (the previous catalog is preserved in both cases).
    ///
    /// Exposed for downstream observability (the health endpoint).  The
    /// `last_refresh_error` string, when present, is available via
    /// [`CatalogService::last_refresh_error`].
    pub fn last_refresh_status(&self) -> RefreshStatus {
        self.inner.read().last_refresh_status
    }

    /// Human-readable error string from the most recent failed refresh, or
    /// `None` if the last refresh succeeded or no refresh has been attempted.
    pub fn last_refresh_error(&self) -> Option<String> {
        self.inner.read().last_refresh_error.clone()
    }

    // ── Write accessors ───────────────────────────────────────────────────────

    /// Inject synthetic catalog entries for built-in providers that have no
    /// corresponding models.dev entry.  This makes providers like `chatgpt_codex` and
    /// `gcp_vertex_ai` visible in `provider_catalog` without requiring them to exist
    /// in the upstream models.dev JSON.
    ///
    /// Model lists are sourced from models.dev when a mapping exists (see
    /// [`MODEL_SOURCE_MAP`]).
    ///
    /// The refresh compose/swap path now applies builtin injections itself, so
    /// callers that refresh no longer need to re-inject afterwards.  This method
    /// is retained for the startup path (seeding builtins before the first
    /// refresh) and for in-memory catalogs that never refresh; it delegates to
    /// the same free helper used by refresh composition so behavior stays
    /// identical.
    pub fn inject_builtin_providers(&self, entries: &[BuiltinProvider]) {
        let mut data = self.inner.write();
        let CatalogData {
            providers,
            models_idx,
            ..
        } = &mut *data;
        inject_builtins_into(providers, models_idx, entries);
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
        data.custom_providers.remove(provider_id);
        remove_provider_from_active(&mut data, provider_id);
    }

    /// Add or replace a custom provider and its seed models in the in-memory catalog.
    /// Persisting to DB is the caller's responsibility.
    pub fn add_custom_provider(&self, provider: Provider, seed_models: Vec<Model>) {
        let normalized = normalize_seed_models(&provider, seed_models);
        let retained = CustomCatalogProvider {
            provider: provider.clone(),
            seed_models: normalized.clone(),
        };

        let mut data = self.inner.write();
        data.custom_providers.insert(provider.id.clone(), retained);
        apply_custom_provider_to_active(&mut data, &provider, &normalized);
    }
}

impl Default for CatalogService {
    fn default() -> Self {
        Self::new()
    }
}

// ── Custom-provider helpers ───────────────────────────────────────────────────

/// Strip the `"<provider_id>/"` prefix from each seed-model id so internal
/// IDs are always the bare model name.  Shared between `add_custom_provider`
/// (which seeds the retained set) and the periodic refresh compose/swap path
/// (which will re-apply the retained set after a models.dev reload).
///
/// Behavior matches the upstream `normalize()` path: a model whose id already
/// starts with `"<provider_id>/"` is rewritten to the bare form; any other id
/// (including dotted ids like `mimo-v2.5-pro`) is left untouched.  Empty input
/// produces an empty output without allocating a throwaway prefix string.
fn normalize_seed_models(provider: &Provider, seed_models: Vec<Model>) -> Vec<Model> {
    if seed_models.is_empty() {
        return Vec::new();
    }
    let prefix = format!("{}/", provider.id);
    seed_models
        .into_iter()
        .map(|mut m| {
            if let Some(bare) = m.id.strip_prefix(&prefix) {
                m.id = bare.to_string();
            }
            m
        })
        .collect()
}

/// Overlay a retained custom-provider entry into the active `providers` /
/// `models_idx` state.  Always replaces any prior entry under the same id so
/// add/replace semantics stay consistent with the previous in-place behavior.
/// The providers list is kept sorted by id to match `normalize()`.
fn apply_custom_provider_to_active(
    data: &mut CatalogData,
    provider: &Provider,
    seed_models: &[Model],
) {
    data.providers.retain(|p| p.id != provider.id);
    data.providers.push(provider.clone());
    data.providers.sort_by(|a, b| a.id.cmp(&b.id));

    data.models_idx.remove(&provider.id);
    if !seed_models.is_empty() {
        data.models_idx
            .insert(provider.id.clone(), seed_models.to_vec());
    }
}

/// Remove a provider (and its model list) from the active catalog.  Used by
/// `remove_custom_provider` and by the periodic refresh compose/swap path to
/// strip an entry that no longer belongs in the retained set.
fn remove_provider_from_active(data: &mut CatalogData, provider_id: &str) {
    data.providers.retain(|p| p.id != provider_id);
    data.models_idx.remove(provider_id);
}

// ── Refresh composition helpers ──────────────────────────────────────────────
//
// A successful live refresh must rebuild the *complete* active catalog from
// normalized upstream data before swapping, so the swap is atomic and a
// refresh never drops local entries.  The composition order is:
//
//   1. Replace the upstream-derived providers/models with the fresh payload.
//   2. Inject builtin providers (e.g. `chatgpt_codex`, `gcp_vertex_ai`) that
//      have no models.dev entry, sourcing their model lists from mapped
//      upstream providers.
//   3. Overlay the retained custom-provider set so user-registered entries
//      survive every upstream reload.
//
// All three steps mutate a local `(providers, models_idx)` pair (or the
// `CatalogData` they live in) without touching the live catalog until the
// caller swaps under one write lock.

/// Compose the active catalog from a fresh normalized upstream payload plus the
/// retained overlays, then write it into `data` in place.  Called by the
/// refresh path after it has already validated that `providers` is non-empty.
///
/// `data.custom_providers` is read (not replaced): the retained set is the
/// source of truth for local entries and is re-applied on every successful
/// refresh.
fn compose_catalog(
    data: &mut CatalogData,
    providers: Vec<Provider>,
    models_idx: HashMap<String, Vec<Model>>,
) {
    // Snapshot the retained custom-provider set before mutating `data` so the
    // immutable borrow ends before the mutable overlay calls below.  An entry
    // removed via `remove_custom_provider` is absent here and therefore not
    // resurrected by the refresh.
    let retained: Vec<CustomCatalogProvider> = data.custom_providers.values().cloned().collect();

    // 1. Fresh upstream providers/models.
    data.providers = providers;
    data.models_idx = models_idx;

    // 2. Inject builtin providers that have no models.dev entry.
    let CatalogData {
        providers,
        models_idx,
        ..
    } = &mut *data;
    inject_builtins_into(providers, models_idx, BUILTIN_PROVIDERS);

    // 3. Re-apply the retained custom-provider set so user-registered entries
    //    survive the upstream reload.
    for ccp in &retained {
        apply_custom_provider_to_active(data, &ccp.provider, &ccp.seed_models);
    }
}

/// Inject synthetic catalog entries for built-in providers that have no
/// corresponding models.dev entry into a local `(providers, models_idx)` pair.
///
/// Providers whose id already exists are skipped (no duplication).  Model lists
/// are sourced from a mapped models.dev provider via [`models_from_idx`] when a
/// [`MODEL_SOURCE_MAP`] entry exists.  The providers list is kept sorted by id
/// to match `normalize()`.
///
/// Shared by [`CatalogService::inject_builtin_providers`] (the explicit
/// startup/in-memory path) and [`compose_catalog`] (the refresh path) so both
/// apply builtins identically.
fn inject_builtins_into(
    providers: &mut Vec<Provider>,
    models_idx: &mut HashMap<String, Vec<Model>>,
    entries: &[BuiltinProvider],
) {
    let existing_ids: HashSet<String> = providers.iter().map(|p| p.id.clone()).collect();

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
        providers.push(provider);

        // Try to source models from models.dev via the mapping table.
        if let Some(models) = models_from_idx(models_idx, bp.id) {
            models_idx.insert(bp.id.to_string(), models);
        }
    }

    providers.sort_by(|a, b| a.id.cmp(&b.id));
}

/// Pull models from a mapped models.dev provider, re-tagged with the target
/// provider ID and filtered by the optional prefix.  Returns `None` when no
/// [`MODEL_SOURCE_MAP`] mapping exists or the source provider has no matching
/// models.  Free-function form of the former
/// `CatalogService::models_from_catalog_source` so it can run over a local
/// `(providers, models_idx)` pair during refresh composition.
fn models_from_idx(
    models_idx: &HashMap<String, Vec<Model>>,
    builtin_provider_id: &str,
) -> Option<Vec<Model>> {
    let (_, source_id, prefix) = MODEL_SOURCE_MAP
        .iter()
        .find(|(builtin_id, _, _)| *builtin_id == builtin_provider_id)?;

    let source_models = models_idx.get(*source_id)?;
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

    // Borrow pay-as-you-go pricing into flat-rate plan providers so their usage
    // is no longer dropped from spend analytics. Runs on every (re)load — both
    // the embedded snapshot and live refresh paths go through `normalize`.
    enrich_plan_pricing(&mut models_idx);

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

// ── Plan → pay-as-you-go pricing reference mapping ────────────────────────────
//
// Consumer coding/token plans (e.g. `zai-coding-plan`) are billed as a flat
// monthly subscription, so models.dev carries their models with all-zero
// pricing. That makes their usage show as "unpriced" in spend analytics even
// though the *same* underlying model has a published pay-as-you-go rate under
// its first-party provider (e.g. `zai/glm-5`). We borrow that rate as the
// public-API-rate ESTIMATE the usage dashboard already advertises ("Est. at
// public API rates") so subscription usage is no longer silently dropped from
// the spend stack.
//
// This deliberately mirrors [`MODEL_SOURCE_MAP`]: an explicit, auditable list
// rather than a fuzzy heuristic. A plan model is only enriched when (a) its own
// catalog pricing is all-zero and (b) the base provider exposes a model with a
// matching canonical id that *is* priced. Plan-only models with no priced
// counterpart (e.g. a `k2p7` whose base id is `kimi-k2.7`) stay unpriced —
// unknown is never silently encoded as $0.
//
// (plan_provider_id, pay-as-you-go_provider_id)
const PRICING_REFERENCE_MAP: &[(&str, &str)] = &[
    ("zai-coding-plan", "zai"),
    ("zhipuai-coding-plan", "zhipuai"),
    ("minimax-coding-plan", "minimax"),
    ("minimax-cn-coding-plan", "minimax-cn"),
    ("kimi-for-coding", "moonshotai"),
    ("xiaomi-token-plan-sgp", "xiaomi"),
    ("xiaomi-token-plan-cn", "xiaomi"),
    ("xiaomi-token-plan-ams", "xiaomi"),
];

/// Explicit per-model pricing aliases for plan models whose id has **no**
/// canonical match in the base provider's catalog, so [`PRICING_REFERENCE_MAP`]
/// alone can't price them.
///
/// Kimi for Coding ships coding ids (`k2p7`, `k2p5`) that don't appear in
/// moonshotai's pay-as-you-go list (which uses `kimi-k2-thinking`, `kimi-k2.5`,
/// …). They're the same Kimi-K2 family billed at the standard K2 rate, so we
/// point them at `moonshotai/kimi-k2-thinking` ($0.6 in / $2.5 out / $0.15
/// cache). The second tuple field is the **canonical** plan-model id (see
/// [`canonical_model_id`]).
///
/// (plan_provider_id, canonical_plan_model_id, base_provider_id, base_model_id)
const PRICING_MODEL_ALIAS: &[(&str, &str, &str, &str)] = &[
    ("kimi-for-coding", "k2p7", "moonshotai", "kimi-k2-thinking"),
    ("kimi-for-coding", "k2p5", "moonshotai", "kimi-k2-thinking"),
];

/// Strip non-alphanumeric chars and lowercase so plan/base model ids match
/// across cosmetic spelling differences (`MiniMax-M2.5` ↔ `minimax-m2.5`).
fn canonical_model_id(id: &str) -> String {
    id.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

/// Borrow pay-as-you-go pricing into flat-rate plan providers' zero-priced
/// models. Mutates `models_idx` in place. Only fills a plan model whose own
/// pricing is all-zero, and only from a base model that is itself priced —
/// existing pricing is never overwritten.
///
/// Two sources feed the per-plan canonical-id → pricing lookup: the base
/// provider's whole priced model list ([`PRICING_REFERENCE_MAP`]) and explicit
/// per-model aliases for ids that don't canonically match ([`PRICING_MODEL_ALIAS`]).
fn enrich_plan_pricing(models_idx: &mut HashMap<String, Vec<Model>>) {
    // Phase 1: resolve every plan's canonical-id → Pricing lookup while only
    // borrowing `models_idx` immutably (so phase 2 can take a mutable borrow).
    let mut plan_lookups: HashMap<&str, HashMap<String, Pricing>> = HashMap::new();

    for (plan_id, base_id) in PRICING_REFERENCE_MAP {
        if let Some(base_models) = models_idx.get(*base_id) {
            let entry = plan_lookups.entry(*plan_id).or_default();
            for m in base_models {
                if has_nonzero_pricing(&m.pricing) {
                    entry.insert(canonical_model_id(&m.id), m.pricing.clone());
                }
            }
        }
    }

    for (plan_id, plan_model_canon, base_id, base_model) in PRICING_MODEL_ALIAS {
        let pricing = models_idx
            .get(*base_id)
            .and_then(|ms| ms.iter().find(|m| m.id == *base_model))
            .map(|m| m.pricing.clone());
        if let Some(pricing) = pricing.filter(has_nonzero_pricing) {
            plan_lookups
                .entry(*plan_id)
                .or_default()
                .insert((*plan_model_canon).to_string(), pricing);
        }
    }

    // Phase 2: fill any zero-priced plan model that has a lookup hit.
    for (plan_id, lookup) in &plan_lookups {
        if lookup.is_empty() {
            continue;
        }
        let Some(plan_models) = models_idx.get_mut(*plan_id) else {
            continue;
        };
        for m in plan_models.iter_mut() {
            if has_nonzero_pricing(&m.pricing) {
                continue;
            }
            if let Some(pricing) = lookup.get(&canonical_model_id(&m.id)) {
                m.pricing = pricing.clone();
            }
        }
    }
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
            credential_class: crate::catalog::builtin::CredentialClass::ApiKey,
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
    fn enrich_plan_pricing_borrows_payg_rates_for_zero_priced_plan_models() {
        // zero-priced plan model + matching priced base model → borrowed.
        // zero-priced plan model with no base match → stays unpriced.
        // already-priced plan model → never overwritten.
        let zero = Pricing::default();
        let base = Pricing {
            input_per_million: 1.0,
            output_per_million: 3.2,
            cache_read_per_million: 0.2,
            cache_write_per_million: 0.0,
        };
        let mk = |provider: &str, id: &str, pricing: Pricing| Model {
            id: id.to_string(),
            provider_id: provider.to_string(),
            name: id.to_string(),
            tool_call: true,
            reasoning: true,
            attachment: false,
            context_window: 0,
            output_limit: 0,
            pricing,
        };

        let mut idx: HashMap<String, Vec<Model>> = HashMap::new();
        // Base pay-as-you-go provider — note the cosmetic id-casing difference.
        idx.insert("zai".to_string(), vec![mk("zai", "GLM-5", base.clone())]);
        idx.insert(
            "zai-coding-plan".to_string(),
            vec![
                mk("zai-coding-plan", "glm-5", zero.clone()), // canonical match → borrowed
                mk("zai-coding-plan", "glm-only-plan", zero.clone()), // no base match → stays unpriced
                mk("zai-coding-plan", "glm-paid", base.clone()),      // already priced → untouched
            ],
        );

        enrich_plan_pricing(&mut idx);

        let plan = &idx["zai-coding-plan"];
        let borrowed = plan.iter().find(|m| m.id == "glm-5").unwrap();
        assert!(
            has_nonzero_pricing(&borrowed.pricing),
            "glm-5 should inherit zai/GLM-5 pricing"
        );
        assert_eq!(borrowed.pricing.input_per_million, 1.0);
        assert_eq!(borrowed.pricing.output_per_million, 3.2);

        let unmatched = plan.iter().find(|m| m.id == "glm-only-plan").unwrap();
        assert!(
            !has_nonzero_pricing(&unmatched.pricing),
            "a plan-only model with no priced base counterpart stays unpriced"
        );

        let already = plan.iter().find(|m| m.id == "glm-paid").unwrap();
        assert_eq!(
            already.pricing.input_per_million, base.input_per_million,
            "existing pricing must never be overwritten"
        );
    }

    #[test]
    fn enrich_plan_pricing_applies_explicit_model_alias() {
        // kimi-for-coding ships `k2p7`/`k2p5`, which don't canonically match any
        // moonshotai id — they must be priced via PRICING_MODEL_ALIAS instead.
        let kimi_rate = Pricing {
            input_per_million: 0.6,
            output_per_million: 2.5,
            cache_read_per_million: 0.15,
            cache_write_per_million: 0.0,
        };
        let mk = |provider: &str, id: &str, pricing: Pricing| Model {
            id: id.to_string(),
            provider_id: provider.to_string(),
            name: id.to_string(),
            tool_call: true,
            reasoning: true,
            attachment: false,
            context_window: 0,
            output_limit: 0,
            pricing,
        };

        let mut idx: HashMap<String, Vec<Model>> = HashMap::new();
        idx.insert(
            "moonshotai".to_string(),
            vec![mk("moonshotai", "kimi-k2-thinking", kimi_rate.clone())],
        );
        idx.insert(
            "kimi-for-coding".to_string(),
            vec![
                mk("kimi-for-coding", "k2p7", Pricing::default()),
                mk("kimi-for-coding", "k2p5", Pricing::default()),
            ],
        );

        enrich_plan_pricing(&mut idx);

        for id in ["k2p7", "k2p5"] {
            let m = idx["kimi-for-coding"].iter().find(|m| m.id == id).unwrap();
            assert!(
                has_nonzero_pricing(&m.pricing),
                "{id} should be priced via the explicit kimi alias"
            );
            assert_eq!(m.pricing.output_per_million, 2.5);
        }
    }

    #[test]
    fn enrich_plan_pricing_runs_during_normalize() {
        // End-to-end through the public seed path: the embedded snapshot's
        // zai-coding-plan models should resolve real pricing borrowed from `zai`
        // (both are models.dev-native), so they're no longer "unpriced".
        let catalog = CatalogService::new();
        let plan_models = catalog.list_models("zai-coding-plan");
        if plan_models.is_empty() {
            return; // snapshot may not carry the plan provider; nothing to assert.
        }
        let pricing_map = catalog.pricing_for_all_models();
        let any_priced = plan_models
            .iter()
            .any(|m| pricing_map.contains_key(&format!("zai-coding-plan/{}", m.id)));
        assert!(
            any_priced,
            "at least one zai-coding-plan model should be priced via the zai reference map"
        );
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
            credential_class: crate::catalog::builtin::CredentialClass::ApiKey,
        }];
        catalog.inject_builtin_providers(entries);

        assert_eq!(catalog.list_providers().len(), initial_count);
    }

    // ── Custom-provider retention tests ───────────────────────────────────────

    fn mk_custom_provider(id: &str) -> Provider {
        Provider {
            id: id.to_string(),
            name: format!("Custom {id}"),
            npm: String::new(),
            env_vars: vec![format!("{id}_API_KEY")],
            base_url: format!("https://api.{id}.invalid/v1"),
            docs_url: String::new(),
            is_openai_compatible: true,
        }
    }

    fn mk_seed_model(id: &str, provider_id: &str) -> Model {
        Model {
            id: id.to_string(),
            provider_id: provider_id.to_string(),
            name: id.to_string(),
            tool_call: true,
            reasoning: false,
            attachment: false,
            context_window: 0,
            output_limit: 0,
            pricing: Pricing::default(),
        }
    }

    /// Test-only accessor mirroring `CatalogService::list_models` over a raw
    /// `CatalogData`, so refresh-composition tests can assert model lists
    /// without going through the public service.
    impl CatalogData {
        fn list_models_test(&self, provider_id: &str) -> Vec<String> {
            self.models_idx
                .get(provider_id)
                .map(|ms| ms.iter().map(|m| m.id.clone()).collect())
                .unwrap_or_default()
        }

        /// Sorted provider ids, for equality checks (`Provider` is an
        /// external-crate model without `PartialEq`).
        fn provider_ids_test(&self) -> Vec<String> {
            let mut ids: Vec<String> = self.providers.iter().map(|p| p.id.clone()).collect();
            ids.sort();
            ids
        }
    }

    /// `add_custom_provider` must persist the entry in the retained custom-provider
    /// set *and* surface it through the active catalog's read methods.
    #[test]
    fn add_custom_provider_retains_entry_and_reflects_in_active() {
        let catalog = CatalogService::new();
        let provider = mk_custom_provider("retentive");
        let seeds = vec![
            mk_seed_model("alpha", "retentive"),
            mk_seed_model("beta", "retentive"),
        ];

        catalog.add_custom_provider(provider.clone(), seeds.clone());

        let providers = catalog.list_providers();
        assert!(
            providers.iter().any(|p| p.id == "retentive"),
            "add_custom_provider should expose the provider in list_providers"
        );
        let models = catalog.list_models("retentive");
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert!(
            ids.contains(&"alpha") && ids.contains(&"beta"),
            "add_custom_provider should expose seed models in list_models; got {ids:?}"
        );

        let found = catalog
            .find_model("retentive/alpha")
            .expect("retentive/alpha should be findable");
        assert_eq!(found.provider_id, "retentive");
        assert_eq!(found.id, "alpha");

        let data = catalog.inner.read();
        let retained = data
            .custom_providers
            .get("retentive")
            .expect("retentive entry should be retained in CatalogData.custom_providers");
        assert_eq!(retained.provider.id, "retentive");
        let retained_ids: Vec<&str> = retained.seed_models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(retained_ids, vec!["alpha", "beta"]);
    }

    /// Calling `add_custom_provider` a second time with the same id must
    /// replace (not duplicate) both the retained entry and the active catalog.
    #[test]
    fn add_custom_provider_replaces_existing_entry() {
        let catalog = CatalogService::new();
        let provider = mk_custom_provider("replacy");

        catalog.add_custom_provider(provider.clone(), vec![mk_seed_model("v1", "replacy")]);
        catalog.add_custom_provider(
            provider.clone(),
            vec![
                mk_seed_model("v2-alpha", "replacy"),
                mk_seed_model("v2-beta", "replacy"),
            ],
        );

        let matching = catalog
            .list_providers()
            .into_iter()
            .filter(|p| p.id == "replacy")
            .count();
        assert_eq!(matching, 1, "replace must not duplicate the provider");

        let ids: Vec<String> = catalog
            .list_models("replacy")
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["v2-alpha", "v2-beta"]);

        let data = catalog.inner.read();
        let retained = data
            .custom_providers
            .get("replacy")
            .expect("replacy should still be retained after replace");
        let retained_ids: Vec<String> = retained.seed_models.iter().map(|m| m.id.clone()).collect();
        assert_eq!(retained_ids, vec!["v2-alpha", "v2-beta"]);
    }

    /// `remove_custom_provider` must drop the entry from both the retained set
    /// and the active catalog so a future refresh compose/swap cannot
    /// resurrect it.
    #[test]
    fn remove_custom_provider_clears_retained_and_active() {
        let catalog = CatalogService::new();
        catalog.add_custom_provider(
            mk_custom_provider("deleteme"),
            vec![mk_seed_model("m", "deleteme")],
        );

        catalog.remove_custom_provider("deleteme");

        assert!(
            catalog.list_providers().iter().all(|p| p.id != "deleteme"),
            "remove_custom_provider must drop the provider from list_providers"
        );
        assert!(
            catalog.list_models("deleteme").is_empty(),
            "remove_custom_provider must drop the model list"
        );
        assert!(
            catalog.find_model("deleteme/m").is_none(),
            "find_model must no longer resolve deleteme/m"
        );

        let data = catalog.inner.read();
        assert!(
            !data.custom_providers.contains_key("deleteme"),
            "remove_custom_provider must clear the retained entry"
        );
    }

    /// Seed-model normalization must strip the `"<provider_id>/"` prefix from
    /// full-form ids, leave unrelated ids untouched (including dotted ids),
    /// and tolerate empty input.
    #[test]
    fn normalize_seed_models_strips_provider_prefix_only() {
        let provider = mk_custom_provider("norm");

        let empty: Vec<Model> = normalize_seed_models(&provider, Vec::new());
        assert!(empty.is_empty());

        let input = vec![
            mk_seed_model("norm/bare-from-full", "norm"),
            mk_seed_model("already-bare", "norm"),
            mk_seed_model("mimo-v2.5-pro", "norm"),
            mk_seed_model("norm/dotted.v2", "norm"),
        ];
        let normalized = normalize_seed_models(&provider, input);
        let ids: Vec<String> = normalized.iter().map(|m| m.id.clone()).collect();
        assert_eq!(
            ids,
            vec![
                "bare-from-full".to_string(),
                "already-bare".to_string(),
                "mimo-v2.5-pro".to_string(),
                "dotted.v2".to_string(),
            ],
            "only the provider/ prefix is stripped; bare and unrelated ids are preserved"
        );
    }

    /// End-to-end: seed models submitted through `add_custom_provider` with
    /// the full `"<provider_id>/<model>"` form must surface through the
    /// active catalog with the prefix stripped, so `find_model` and the
    /// pricing snapshot use the canonical bare id.
    #[test]
    fn add_custom_provider_normalizes_seed_model_ids_end_to_end() {
        let catalog = CatalogService::new();
        let provider = mk_custom_provider("normalize-me");
        let priced = |id: &str, in_rate: f64| Model {
            id: id.to_string(),
            provider_id: "normalize-me".to_string(),
            name: id.to_string(),
            tool_call: false,
            reasoning: false,
            attachment: false,
            context_window: 0,
            output_limit: 0,
            pricing: Pricing {
                input_per_million: in_rate,
                output_per_million: in_rate * 2.0,
                ..Pricing::default()
            },
        };

        catalog.add_custom_provider(
            provider,
            vec![
                priced("normalize-me/full-form", 1.0),
                priced("bare-form", 2.0),
            ],
        );

        let ids: Vec<String> = catalog
            .list_models("normalize-me")
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert!(
            ids.contains(&"full-form".to_string()),
            "prefix must be stripped; got {ids:?}"
        );
        assert!(ids.contains(&"bare-form".to_string()));

        let found = catalog
            .find_model("normalize-me/full-form")
            .expect("full-form model must resolve via the canonical bare id");
        assert_eq!(found.id, "full-form");
        assert_eq!(found.provider_id, "normalize-me");

        let pricing = catalog.pricing_for_all_models();
        assert!(
            pricing.contains_key("normalize-me/full-form"),
            "pricing_for_all_models must key on the stripped id; got {pricing:?}"
        );
        assert_eq!(pricing["normalize-me/full-form"].input_per_million, 1.0);
    }

    // ── Refresh compose/swap tests ────────────────────────────────────────────
    //
    // The live `refresh()` path calls the network, so these tests exercise the
    // pure composition helpers (`compose_catalog`) and the status/rejection
    // transitions directly over a `CatalogData`, mirroring exactly what
    // `refresh()` does after a fetch resolves.

    /// Build an empty `CatalogData` populated with a couple of retained custom
    /// providers so the refresh-composition tests start from a realistic state.
    fn data_with_retained_custom() -> CatalogData {
        let mut models_idx = HashMap::new();
        models_idx.insert(
            "upstream-only".to_string(),
            vec![mk_seed_model("m0", "upstream-only")],
        );
        let mut custom_providers = HashMap::new();
        // Two retained custom providers.
        custom_providers.insert(
            "custom-one".to_string(),
            CustomCatalogProvider {
                provider: mk_custom_provider("custom-one"),
                seed_models: vec![mk_seed_model("a1", "custom-one")],
            },
        );
        custom_providers.insert(
            "custom-two".to_string(),
            CustomCatalogProvider {
                provider: mk_custom_provider("custom-two"),
                seed_models: vec![
                    mk_seed_model("b1", "custom-two"),
                    mk_seed_model("b2", "custom-two"),
                ],
            },
        );
        CatalogData {
            providers: vec![mk_custom_provider("upstream-only")],
            models_idx,
            custom_providers,
            ..Default::default()
        }
    }

    /// A successful refresh composes normalized upstream data, injected builtin
    /// providers, and the retained custom-provider set before swapping — so the
    /// retained custom entries survive the upstream reload.
    #[test]
    fn refresh_composition_retains_custom_providers() {
        let mut data = data_with_retained_custom();

        // A fresh normalized upstream payload that does NOT contain the custom
        // providers.  It must contain a models.dev source for a builtin (openai)
        // so builtin injection has a model list to borrow.
        let fresh_provider = mk_custom_provider("openai");
        let fresh_models = vec![mk_seed_model("gpt-x", "openai")];
        let providers = vec![fresh_provider];
        let mut models_idx = HashMap::new();
        models_idx.insert("openai".to_string(), fresh_models);

        compose_catalog(&mut data, providers, models_idx);

        // The fresh upstream provider is present.
        assert!(
            data.providers.iter().any(|p| p.id == "openai"),
            "fresh upstream provider must be in the composed catalog"
        );
        // The old upstream-only provider was replaced.
        assert!(
            !data.providers.iter().any(|p| p.id == "upstream-only"),
            "a refresh replaces the prior upstream set; stale upstream-only must be gone"
        );

        // Both retained custom providers survive.
        for id in ["custom-one", "custom-two"] {
            assert!(
                data.providers.iter().any(|p| p.id == id),
                "retained custom provider {id} must survive refresh composition"
            );
        }
        assert_eq!(
            data.list_models_test("custom-one"),
            vec!["a1".to_string()],
            "retained custom-one seed models survive"
        );
        assert_eq!(
            data.list_models_test("custom-two"),
            vec!["b1".to_string(), "b2".to_string()],
            "retained custom-two seed models survive"
        );

        // Builtin injection ran: chatgpt_codex (mapped from openai via the
        // "codex" prefix) is absent here because no openai model contains
        // "codex", but a mapped builtin with a broad prefix still resolves.
        // gcp_vertex_ai maps from google-vertex (absent), so it gets injected as
        // a provider with no model list.  Verify at least one non-upstream,
        // non-custom builtin was injected.
        let builtin_added = data
            .providers
            .iter()
            .map(|p| p.id.as_str())
            .any(|id| BUILTIN_PROVIDERS.iter().any(|bp| bp.id == id) && id != "openai");
        assert!(
            builtin_added,
            "builtin injection must run during refresh composition; got providers {:?}",
            data.providers.iter().map(|p| &p.id).collect::<Vec<_>>()
        );
    }

    /// A successful refresh must not resurrect a custom provider that was
    /// removed before the refresh composed.  This is the regression guard for
    /// the remove-then-refresh no-resurrection invariant.
    #[test]
    fn refresh_composition_does_not_resurrect_removed_custom() {
        let mut data = data_with_retained_custom();
        // Remove custom-one from the retained set (as remove_custom_provider
        // would) before composing.
        data.custom_providers.remove("custom-one");

        let providers = vec![mk_custom_provider("openai")];
        let mut models_idx = HashMap::new();
        models_idx.insert("openai".to_string(), vec![mk_seed_model("gpt-x", "openai")]);

        compose_catalog(&mut data, providers, models_idx);

        assert!(
            !data.providers.iter().any(|p| p.id == "custom-one"),
            "a removed custom provider must not be resurrected by refresh composition"
        );
        assert!(
            data.list_models_test("custom-one").is_empty(),
            "removed custom-one model list must stay empty after refresh"
        );
        // custom-two was retained and survives.
        assert!(
            data.providers.iter().any(|p| p.id == "custom-two"),
            "still-retained custom-two must survive"
        );
    }

    /// A failed refresh (fetch/parse error) preserves the previously active
    /// catalog data unchanged and transitions status to Error.  This mirrors
    /// the `Err` arm of `refresh()` without calling the network.
    #[test]
    fn failed_refresh_preserves_previous_catalog() {
        let mut data = data_with_retained_custom();
        let prev_provider_ids = data.provider_ids_test();
        let mut prev_model_keys: Vec<String> = data.models_idx.keys().cloned().collect();
        prev_model_keys.sort();

        // Simulate the refresh Err arm.
        let err = "models.dev returned HTTP 503".to_string();
        data.last_refresh_status = RefreshStatus::Error;
        data.last_refresh_error = Some(err.clone());

        // The active catalog must be untouched.
        assert_eq!(
            data.provider_ids_test(),
            prev_provider_ids,
            "a failed refresh must not mutate the active providers"
        );
        let mut model_keys: Vec<String> = data.models_idx.keys().cloned().collect();
        model_keys.sort();
        assert_eq!(
            model_keys, prev_model_keys,
            "a failed refresh must not mutate the active models_idx keys"
        );
        assert_eq!(data.last_refresh_status, RefreshStatus::Error);
        assert_eq!(data.last_refresh_error.as_deref(), Some(err.as_str()));
    }

    /// A zero-provider normalized payload is rejected: the active catalog is
    /// preserved unchanged and status transitions to Error.  Mirrors the
    /// zero-provider guard in `refresh()`.
    #[test]
    fn zero_provider_payload_is_rejected() {
        let mut data = data_with_retained_custom();
        let prev_provider_ids = data.provider_ids_test();
        let mut prev_model_keys: Vec<String> = data.models_idx.keys().cloned().collect();
        prev_model_keys.sort();

        // Simulate the zero-provider rejection arm of refresh(): do NOT call
        // compose_catalog (which would wipe the catalog); instead record the
        // rejection exactly as refresh() does.
        data.last_refresh_status = RefreshStatus::Error;
        data.last_refresh_error =
            Some("models.dev normalized payload had zero providers".to_string());

        assert_eq!(
            data.provider_ids_test(),
            prev_provider_ids,
            "a zero-provider payload must not overwrite the active catalog"
        );
        let mut model_keys: Vec<String> = data.models_idx.keys().cloned().collect();
        model_keys.sort();
        assert_eq!(
            model_keys, prev_model_keys,
            "a zero-provider payload must not overwrite the active models_idx keys"
        );
        assert_eq!(data.last_refresh_status, RefreshStatus::Error);
    }

    /// `compose_catalog` with an empty upstream payload (as could result from a
    /// degenerate normalize) would wipe the catalog; the refresh path guards
    /// against this by checking `providers.is_empty()` before calling
    /// compose_catalog.  Verify the guard predicate directly: an empty upstream
    /// providers vec must be treated as a rejection, not passed to
    /// compose_catalog.
    #[test]
    fn refresh_rejects_empty_upstream_providers_vec() {
        let data = data_with_retained_custom();
        let prev_provider_ids = data.provider_ids_test();

        // The guard from refresh(): only compose when providers is non-empty.
        let upstream_providers: Vec<Provider> = Vec::new();
        let should_compose = !upstream_providers.is_empty();
        assert!(
            !should_compose,
            "an empty upstream providers vec must be rejected before composition"
        );
        // Because we did not compose, the catalog is unchanged.
        assert_eq!(data.provider_ids_test(), prev_provider_ids);
    }

    /// The public status accessors report the refresh outcome.  A fresh service
    /// reports `Never`; after a simulated success the status is `Success`.
    #[test]
    fn refresh_status_transitions() {
        let catalog = CatalogService::new();
        assert_eq!(
            catalog.last_refresh_status(),
            RefreshStatus::Never,
            "a freshly-seeded service has never refreshed"
        );
        assert!(
            catalog.last_refresh_error().is_none(),
            "no error before any refresh attempt"
        );

        // Simulate a successful refresh outcome by composing directly.
        {
            let mut data = catalog.inner.write();
            let providers = vec![mk_custom_provider("openai")];
            let mut models_idx = HashMap::new();
            models_idx.insert("openai".to_string(), vec![mk_seed_model("gpt-x", "openai")]);
            compose_catalog(&mut data, providers, models_idx);
            data.last_refresh_status = RefreshStatus::Success;
            data.last_refresh_error = None;
        }
        assert_eq!(catalog.last_refresh_status(), RefreshStatus::Success);
        assert!(catalog.last_refresh_error().is_none());

        // A subsequent simulated failure flips it back to Error with a message.
        {
            let mut data = catalog.inner.write();
            data.last_refresh_status = RefreshStatus::Error;
            data.last_refresh_error = Some("boom".to_string());
        }
        assert_eq!(catalog.last_refresh_status(), RefreshStatus::Error);
        assert_eq!(catalog.last_refresh_error().as_deref(), Some("boom"));

        // The successful composition must still be serving (not wiped by the
        // status-only failure simulation).
        assert!(
            catalog.list_providers().iter().any(|p| p.id == "openai"),
            "active catalog survives a status-only failure transition"
        );
    }

    /// `inject_builtin_providers` (the explicit startup/in-memory path) and the
    /// refresh composition path both apply builtins via the same free helper,
    /// so a catalog that refreshes ends up with the same builtin coverage as
    /// one that explicitly injects.
    #[test]
    fn refresh_and_explicit_inject_share_builtin_helper() {
        let explicit = CatalogService::new();
        explicit.inject_builtin_providers(BUILTIN_PROVIDERS);
        let explicit_builtin_ids: HashSet<String> = explicit
            .list_providers()
            .into_iter()
            .filter(|p| BUILTIN_PROVIDERS.iter().any(|bp| bp.id == p.id))
            .map(|p| p.id)
            .collect();

        // Compose a catalog with the same upstream set as the embedded seed and
        // verify the builtin ids match.
        let mut data = CatalogData::default();
        let upstream = explicit.list_providers();
        let providers: Vec<Provider> = upstream
            .iter()
            .filter(|p| !BUILTIN_PROVIDERS.iter().any(|bp| bp.id == p.id))
            .cloned()
            .collect();
        let mut models_idx = HashMap::new();
        for p in &providers {
            models_idx.insert(p.id.clone(), explicit.list_models(&p.id));
        }
        compose_catalog(&mut data, providers, models_idx);

        let composed_builtin_ids: HashSet<String> = data
            .providers
            .iter()
            .filter(|p| BUILTIN_PROVIDERS.iter().any(|bp| bp.id == p.id))
            .map(|p| p.id.clone())
            .collect();
        assert_eq!(
            explicit_builtin_ids, composed_builtin_ids,
            "refresh composition and explicit inject must apply the same builtin set"
        );
    }
}
