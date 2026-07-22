use serde::{Deserialize, Serialize};

/// A single LLM provider from the models.dev catalog or a custom registry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub npm: String,
    pub env_vars: Vec<String>,
    pub base_url: String,
    pub docs_url: String,
    pub is_openai_compatible: bool,
}

/// Per-million-token pricing for a model in USD.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Pricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cache_read_per_million: f64,
    pub cache_write_per_million: f64,
}

impl Pricing {
    /// `true` when at least one per-million rate is non-zero.
    ///
    /// An all-zero `Pricing` is what the catalog carries for flat-rate
    /// subscription / coding-plan models (billed monthly, so models.dev reports
    /// $0 per token). Cost-basis classification treats such a snapshot as
    /// *unpriced* rather than a real $0.00 spend: a zero-priced subscription
    /// session must not book as `"projected"` $0.00, and a zero-priced metered
    /// session must not book as `"actual"` $0.00.
    pub fn is_priced(&self) -> bool {
        self.input_per_million != 0.0
            || self.output_per_million != 0.0
            || self.cache_read_per_million != 0.0
            || self.cache_write_per_million != 0.0
    }
}

/// A single model from the catalog.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub provider_id: String,
    pub name: String,
    pub tool_call: bool,
    pub reasoning: bool,
    pub attachment: bool,
    pub context_window: i64,
    pub output_limit: i64,
    pub pricing: Pricing,
}

/// A seed model for custom providers — minimal info to pre-populate the picker.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SeedModel {
    pub id: String,
    pub name: String,
}

/// A user-registered OpenAI-compatible provider stored in the `custom_providers` table.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx", derive(sqlx::FromRow))]
pub struct CustomProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub env_var: String,
    pub seed_models: Vec<SeedModel>,
    pub created_at: String,
}
