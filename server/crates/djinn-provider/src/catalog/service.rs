use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use djinn_core::clock::{Clock, SystemClock};
use djinn_core::models::{Credential, Model, Pricing, Provider};
use parking_lot::RwLock;
use serde::Deserialize;

use crate::catalog::builtin::{BUILTIN_PROVIDERS, BuiltinProvider};

const CATALOG_URL: &str = "https://models.dev/api.json";
const CATALOG_URL_ENV: &str = "DJINN_PROVIDER_CATALOG_URL";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Default freshness window used when computing the source tier for structured
/// log fields inside `refresh()`.  This is intentionally a service-local default
/// for log context only — the health endpoint and callers own the real policy.
const REFRESH_LOG_MAX_AGE: Duration = Duration::from_secs(60 * 60); // 1 hour

fn has_nonzero_pricing(pricing: &Pricing) -> bool {
    // Single source of truth: `Pricing::is_priced` (djinn-core). Kept as a
    // free fn so it can still be passed as a fn-pointer to `Option::filter`.
    pricing.is_priced()
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

/// Freshness tier of the currently served catalog data, derived from the last
/// successful live fetch.  Exposed for downstream observability (the health
/// endpoint) so consumers can distinguish "serving fresh live data" from
/// "serving stale or embedded data while refresh attempts are failing".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SourceTier {
    /// Only embedded/seeded data is served — no live refresh has succeeded yet.
    #[default]
    Embedded,
    /// Live `models.dev` data is served and the last successful fetch is within
    /// the freshness window.
    Live,
    /// A live fetch previously succeeded but the data is now older than the
    /// freshness window (the most recent refreshes may be failing while the
    /// previous catalog is still being served unchanged).
    Stale,
}

#[derive(Default)]
struct CatalogData {
    providers: Vec<Provider>,
    models_idx: HashMap<String, Vec<Model>>,
    /// Retained custom-provider set, kept separate from upstream/builtin catalog
    /// data so a live `models.dev` refresh can recompose the active catalog
    /// without dropping user-registered entries.
    custom_providers: HashMap<String, CustomCatalogProvider>,
    /// Monotonic time of the last successful live `models.dev` refresh.  This is
    /// the sole basis for age and [`SourceTier`] calculations; it is never set by
    /// embedded seeding and persists across failed refreshes.
    fetched_at: Option<Instant>,
    /// Wall-clock time of the same last successful live `models.dev` refresh,
    /// captured atomically with `fetched_at`.  Used for RFC3339 observability
    /// downstream; never set by embedded seeding and persists across failed
    /// refreshes.  Stored as `SystemTime` because it is the idiomatic clock for
    /// wall timestamps and is panic-free when converted to `OffsetDateTime`.
    fetched_at_wall: Option<SystemTime>,
    /// Outcome of the most recent live refresh attempt (`Never` until the first
    /// successful refresh).  Failed or rejected refreshes transition to `Error`
    /// without touching the active catalog.
    last_refresh_status: RefreshStatus,
    /// Human-readable error string from the most recent failed refresh.  Cleared
    /// on a successful refresh.  `None` while `last_refresh_status == Never`.
    last_refresh_error: Option<String>,
}

/// Compute the source tier from the raw `CatalogData` state without going through
/// the public `CatalogService::source_tier` accessor (which would require a
/// second read lock while the caller already holds a write lock).
fn source_tier_from_data(data: &CatalogData, max_age: Duration) -> SourceTier {
    let Some(fetched_at) = data.fetched_at else {
        return SourceTier::Embedded;
    };
    if SystemClock::new()
        .now_instant()
        .saturating_duration_since(fetched_at)
        <= max_age
    {
        SourceTier::Live
    } else {
        SourceTier::Stale
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
            Ok(value) => self.refresh_from_json(value).await,
            Err(e) => {
                let mut data = self.inner.write();
                data.last_refresh_status = RefreshStatus::Error;
                data.last_refresh_error = Some(e.clone());
                tracing::warn!(
                    status = ?data.last_refresh_status,
                    source_tier = ?source_tier_from_data(&data, REFRESH_LOG_MAX_AGE),
                    providers = data.providers.len(),
                    error = %e,
                    "catalog refresh failed — using cached/embedded data"
                );
            }
        }
    }

    /// Deterministic test seam that drives the same normalize/compose/swap path
    /// as a live `refresh()` but with a caller-supplied JSON payload. Kept as an
    /// internal helper so the only public entry point to the live refresh path is
    /// [`refresh`](Self::refresh); integration tests should drive that public path
    /// with a mocked upstream URL rather than bypassing it here.
    async fn refresh_from_json(&self, value: serde_json::Value) {
        let raw = match serde_json::from_value::<HashMap<String, RawProvider>>(value) {
            Ok(raw) => raw,
            Err(e) => {
                let mut data = self.inner.write();
                data.last_refresh_status = RefreshStatus::Error;
                data.last_refresh_error = Some(format!("models.dev JSON parse error: {e}"));
                tracing::warn!(
                    status = ?data.last_refresh_status,
                    source_tier = ?source_tier_from_data(&data, REFRESH_LOG_MAX_AGE),
                    providers = data.providers.len(),
                    error = %e,
                    "catalog refresh failed — using cached/embedded data"
                );
                return;
            }
        };

        let (providers, models_idx) = normalize(raw);
        // Reject an empty normalized upstream payload so a degenerate
        // fetch never overwrites the active catalog with nothing.
        if providers.is_empty() {
            let mut data = self.inner.write();
            data.last_refresh_status = RefreshStatus::Error;
            data.last_refresh_error =
                Some("models.dev normalized payload had zero providers".to_string());
            tracing::warn!(
                status = ?data.last_refresh_status,
                source_tier = ?source_tier_from_data(&data, REFRESH_LOG_MAX_AGE),
                providers = data.providers.len(),
                error = "models.dev normalized payload had zero providers",
                "catalog refresh rejected zero-provider payload — keeping active catalog"
            );
            return;
        }
        let clock = SystemClock::new();
        let now = clock.now_instant();
        let now_wall = clock.now();
        let provider_count = providers.len();
        let model_count: usize = models_idx.values().map(Vec::len).sum();
        // Compose the full catalog (upstream + builtins + retained
        // custom providers) and swap it in under a single write lock.
        let mut data = self.inner.write();
        compose_catalog(&mut data, providers, models_idx);
        data.fetched_at = Some(now);
        data.fetched_at_wall = Some(now_wall);
        data.last_refresh_status = RefreshStatus::Success;
        data.last_refresh_error = None;
        tracing::info!(
            status = ?data.last_refresh_status,
            source_tier = ?SourceTier::Live,
            providers = provider_count,
            models = model_count,
            "provider catalog refreshed from models.dev"
        );
    }

    /// Test-only override surface for the upstream models.dev URL. Production
    /// code always falls back to `CATALOG_URL`; integration tests can set
    /// `DJINN_PROVIDER_CATALOG_URL` to a local mock server so the real
    /// `refresh()` loop fetches a mocked payload instead of calling the live
    /// internet endpoint.
    fn catalog_url() -> String {
        std::env::var(CATALOG_URL_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| CATALOG_URL.to_string())
    }

    async fn fetch_remote(&self) -> Result<serde_json::Value, String> {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;

        let url = Self::catalog_url();
        let resp = client.get(url).send().await.map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("models.dev returned HTTP {}", resp.status()));
        }

        resp.json::<serde_json::Value>()
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

    /// Wall-clock time of the last successful live `models.dev` fetch, or `None`
    /// when no live fetch has ever succeeded.  Captured at the same commit as
    /// the monotonic `fetched_at` value and preserved across failed refreshes.
    ///
    /// Returns an owned, `Copy`-able value suitable for downstream RFC3339
    /// formatting (e.g. `time::format_description::well_known::Rfc3339`).
    pub fn last_successful_fetch_time(&self) -> Option<SystemTime> {
        self.inner.read().fetched_at_wall
    }

    /// Elapsed time since the last successful live `models.dev` fetch, or `None`
    /// when no live fetch has ever succeeded (only embedded/seeded data is
    /// served).  Uses the monotonic clock so the value is stable across wall-clock
    /// adjustments.
    ///
    /// Exposed for downstream observability (the health endpoint) and as the
    /// building block for [`CatalogService::source_tier`].
    pub fn last_successful_fetch_age(&self) -> Option<Duration> {
        self.inner.read().fetched_at.map(|t| {
            SystemClock::new()
                .now_instant()
                .saturating_duration_since(t)
        })
    }

    /// Compute the freshness tier of the currently served catalog data given a
    /// maximum age for "live" data.
    ///
    /// - [`SourceTier::Embedded`] — no live fetch has ever succeeded.
    /// - [`SourceTier::Live`] — the last successful fetch is within `max_age`.
    /// - [`SourceTier::Stale`] — a fetch previously succeeded but the data is
    ///   now older than `max_age` (recent refreshes may be failing while the
    ///   previous catalog is still served unchanged).
    ///
    /// The `max_age` boundary is caller-supplied so the refresh loop (or health
    /// endpoint) owns the freshness policy rather than the catalog service.
    pub fn source_tier(&self, max_age: Duration) -> SourceTier {
        let Some(age) = self.last_successful_fetch_age() else {
            return SourceTier::Embedded;
        };
        if age <= max_age {
            SourceTier::Live
        } else {
            SourceTier::Stale
        }
    }

    /// Test-only helper to seed the monotonic timestamp of the last successful
    /// live fetch and its status/error. This lets downstream health/observability
    /// tests exercise embedded/live/stale semantics without hitting models.dev.
    ///
    /// Not used in production code; exposed as `pub` so dependent crates can
    /// drive catalog state in their own tests (dependent crates do not see
    /// `#[cfg(test)]` helpers of a library dependency).
    pub fn set_last_success_for_tests(
        &self,
        fetched_at: Option<Instant>,
        status: RefreshStatus,
        error: Option<String>,
    ) {
        let mut data = self.inner.write();
        data.fetched_at = fetched_at;
        data.fetched_at_wall = fetched_at.map(|i| {
            // Best-effort deterministic wall estimate: one nanosecond of simulated
            // monotonic time maps to one nanosecond of wall time since the Unix
            // epoch. This is an arbitrary but stable mapping so tests can format
            // a predictable RFC3339 string without relying on real time.
            let nanos = u64::try_from(i.elapsed().as_nanos()).unwrap_or_default();
            SystemTime::UNIX_EPOCH + Duration::from_nanos(nanos)
        });
        data.last_refresh_status = status;
        data.last_refresh_error = error;
    }

    /// Test-only helper to seed *both* the monotonic and wall-clock timestamps of
    /// the last successful live fetch.  This lets dependent-crate endpoint tests
    /// set a deterministic wall-clock value (e.g. for RFC3339 serialization) and a
    /// deterministic monotonic value without network access.
    ///
    /// Not used in production code; exposed as `pub` so dependent crates can drive
    /// catalog state in their own tests.
    pub fn set_last_success_times_for_tests(
        &self,
        fetched_at: Option<Instant>,
        fetched_at_wall: Option<SystemTime>,
        status: RefreshStatus,
        error: Option<String>,
    ) {
        let mut data = self.inner.write();
        data.fetched_at = fetched_at;
        data.fetched_at_wall = fetched_at_wall;
        data.last_refresh_status = status;
        data.last_refresh_error = error;
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

    /// Replace the *entire* retained custom-provider set from a caller-supplied
    /// collection (e.g. DB rows), normalizing seed-model IDs and updating the
    /// active catalog deterministically under one write lock.
    ///
    /// Providers absent from the supplied collection are removed from both the
    /// retained set and the active catalog; providers present are added or
    /// replaced.  This is the deterministic startup/DB-reload reconciliation
    /// surface that removes the fragile pattern of calling
    /// [`add_custom_provider`](Self::add_custom_provider) in a loop.
    pub fn set_custom_providers(&self, providers: Vec<(Provider, Vec<Model>)>) {
        let new_entries: HashMap<String, CustomCatalogProvider> = providers
            .into_iter()
            .map(|(p, seeds)| {
                let normalized = normalize_seed_models(&p, seeds);
                let retained = CustomCatalogProvider {
                    provider: p,
                    seed_models: normalized,
                };
                (retained.provider.id.clone(), retained)
            })
            .collect();

        let mut data = self.inner.write();

        // Remove from the active catalog any custom providers that are absent
        // from the new set so deleted DB rows do not persist.
        let old_ids: Vec<String> = data.custom_providers.keys().cloned().collect();
        for id in &old_ids {
            if !new_entries.contains_key(id) {
                remove_provider_from_active(&mut data, id);
            }
        }

        // Replace the retained set.
        data.custom_providers = new_entries;

        // Overlay each new entry onto the active catalog.  Clone the values
        // first so the immutable borrow of `data.custom_providers` ends before
        // the mutable `apply_custom_provider_to_active` calls.
        let retained: Vec<CustomCatalogProvider> =
            data.custom_providers.values().cloned().collect();
        for ccp in &retained {
            apply_custom_provider_to_active(&mut data, &ccp.provider, &ccp.seed_models);
        }
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
/// Kimi for Coding ships coding ids (`k3`, `k2p7`, `k2p5`, and the newer
/// upstream `kimi-for-coding` / `kimi-for-coding-highspeed`) that don't
/// canonically match a moonshotai pay-as-you-go id, so we pin each to its true
/// counterpart in moonshotai's list:
///   • `k3`  → `moonshotai/kimi-k3`             ($3 in / $15 out / $0.3 cache)
///   • `k2p7`→ `moonshotai/kimi-k2.7-code`      ($0.95 / $4 / $0.19)
///   • `k2p5`→ `moonshotai/kimi-k2.5`           ($0.6 / $3 / $0.1)
///   • `kimi-for-coding`           → `moonshotai/kimi-k2.7-code`
///   • `kimi-for-coding-highspeed` → `moonshotai/kimi-k2.7-code-highspeed`
///
/// `k2p7`/`k2p5` previously stood in on `kimi-k2-thinking` because their true
/// counterparts did not yet exist upstream; models.dev now carries them, so the
/// aliases point at the exact plan-equivalent models. The second tuple field is
/// the **canonical** plan-model id (see [`canonical_model_id`]) — lowercase
/// alphanumeric only, so `kimi-for-coding` → `kimiforcoding`.
///
/// (plan_provider_id, canonical_plan_model_id, base_provider_id, base_model_id)
const PRICING_MODEL_ALIAS: &[(&str, &str, &str, &str)] = &[
    ("kimi-for-coding", "k3", "moonshotai", "kimi-k3"),
    ("kimi-for-coding", "k2p7", "moonshotai", "kimi-k2.7-code"),
    ("kimi-for-coding", "k2p5", "moonshotai", "kimi-k2.5"),
    (
        "kimi-for-coding",
        "kimiforcoding",
        "moonshotai",
        "kimi-k2.7-code",
    ),
    (
        "kimi-for-coding",
        "kimiforcodinghighspeed",
        "moonshotai",
        "kimi-k2.7-code-highspeed",
    ),
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

// Tests live in a sibling file (`service_tests.rs`) to keep this module under the
// repo source-size guard. It is still a child module of `service`, so it can
// reach these private items via `use super::*`.
#[cfg(test)]
#[allow(clippy::disallowed_methods)] // test: real time for timing assertions
#[path = "service_tests.rs"]
mod tests;
