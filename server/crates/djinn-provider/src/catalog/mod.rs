pub mod builtin;
pub mod health;
pub mod refresh;
pub mod service;
pub mod validate;

pub use health::{HealthKey, HealthTracker, ModelHealth};
pub use refresh::{ProviderCatalogRefreshTicks, run_provider_catalog_refresh_loop};
pub use service::{CatalogService, RefreshStatus, SourceTier};

/// The active catalog, as `djinn-telemetry` needs to see it.
///
/// Model-turn telemetry may only carry provider/model label values the active
/// catalog actually resolves. Implementing the capability here — rather than
/// letting each emitter decide — means a route that stopped resolving simply
/// produces no labels, so it is rejected before emission rather than filtered
/// after it.
impl djinn_telemetry::model_turn_metrics::ModelTurnCatalogV1 for CatalogService {
    fn resolves(&self, provider_id: &str, model_id: &str) -> bool {
        self.find_model(&format!("{provider_id}/{model_id}"))
            .is_some_and(|model| model.provider_id == provider_id && model.id == model_id)
    }
}
