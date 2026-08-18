//! Bounded model-turn admission telemetry with a closed label allow-list
//! (task `75iz`, Phase D).
//!
//! No `model_turn*` metric existed anywhere before this module; this is not an
//! extension of something already emitting.
//!
//! # Why an allow-list constant rather than a redaction rule
//!
//! The original acceptance wording — "telemetry without secrets or raw
//! credential/account/project/user/request/lease IDs" — is a universal negative
//! over every emission the process will ever make. A test run samples some
//! emissions and says nothing about the rest, so no merge-time run can settle
//! it.
//!
//! [`MODEL_TURN_SERIES`] closes the shape instead. It declares every metric
//! name, every label key, and the complete value set of every discriminating
//! key. Two things then become provable rather than sampled:
//!
//! * [`crate::model_turn_metrics::tests`] pattern-checks the constant itself,
//!   so the redaction claim holds for every row the constant can produce.
//! * A capture test compares the *emitted* `(metric, key, value)` set against
//!   the constant expanded for its fixture, so a new key or a new enum value
//!   fails until the constant is updated.
//!
//! The only label values not enumerated in the constant are the three route
//! labels, and those are closed by construction rather than by enumeration:
//! [`ModelTurnRouteLabels`] can only be built for a `(provider, model)` pair
//! the active catalog resolves, and pool identity is carried as the opaque
//! numeric `pool_id` — never a credential, account, project, user, request, or
//! lease identifier.

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};

// ── Metric names ───────────────────────────────────────────────────────────

pub const MODEL_TURN_POOL_TARGET: &str = "djinn_model_turn_pool_target";
pub const MODEL_TURN_IN_FLIGHT: &str = "djinn_model_turn_in_flight";
pub const MODEL_TURN_RESERVATION_DIVERGENCE: &str = "djinn_model_turn_reservation_divergence";
pub const MODEL_TURN_AGGREGATE_OUTPUT_RATE: &str = "djinn_model_turn_aggregate_output_rate";
pub const MODEL_TURN_STREAM_OUTPUT_RATE: &str = "djinn_model_turn_stream_output_rate";
pub const MODEL_TURN_TTFT_SECONDS: &str = "djinn_model_turn_ttft_seconds";
pub const MODEL_TURN_THROTTLES_TOTAL: &str = "djinn_model_turn_throttles_total";
pub const MODEL_TURN_EXPIRY_OUTCOMES_TOTAL: &str = "djinn_model_turn_expiry_outcomes_total";
pub const MODEL_TURN_IDENTITY_ELIGIBILITY: &str = "djinn_model_turn_identity_eligibility";
pub const MODEL_TURN_PROTOCOL_COVERAGE: &str = "djinn_model_turn_protocol_coverage";

// ── Label keys ─────────────────────────────────────────────────────────────

/// Pool identity, and the only `*_id` key in the whole allow-list. Its value is
/// the opaque numeric primary key of `model_turn_pools` — a row the admission
/// ledger created for one credential/provider/model route, carrying none of
/// those identities itself.
pub const LABEL_POOL_ID: &str = "pool_id";
pub const LABEL_PROVIDER: &str = "provider";
pub const LABEL_MODEL: &str = "model";
pub const LABEL_BUCKET: &str = "bucket";
pub const LABEL_OUTCOME: &str = "outcome";

/// The route keys every series carries.
pub const ROUTE_LABEL_KEYS: [&str; 3] = [LABEL_POOL_ID, LABEL_PROVIDER, LABEL_MODEL];

// ── The closed series declaration ──────────────────────────────────────────

/// One emitted series: its metric name, its label keys, and — for the key that
/// discriminates within the series — the complete value set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelTurnSeriesSpecV1 {
    pub metric: &'static str,
    /// The discriminating label key beyond the route keys, if any.
    pub discriminator: Option<&'static str>,
    /// The complete value set of `discriminator`. Empty when there is none.
    pub discriminator_values: &'static [&'static str],
    pub description: &'static str,
}

/// Every model-turn series this process will emit. This is the allow-list.
pub const MODEL_TURN_SERIES: [ModelTurnSeriesSpecV1; 10] = [
    ModelTurnSeriesSpecV1 {
        metric: MODEL_TURN_POOL_TARGET,
        discriminator: None,
        discriminator_values: &[],
        description: "Learned concurrency target for one model-turn pool.",
    },
    ModelTurnSeriesSpecV1 {
        metric: MODEL_TURN_IN_FLIGHT,
        discriminator: None,
        discriminator_values: &[],
        description: "Turns currently in flight against one model-turn pool.",
    },
    ModelTurnSeriesSpecV1 {
        metric: MODEL_TURN_RESERVATION_DIVERGENCE,
        discriminator: None,
        discriminator_values: &[],
        description: "Reserved-minus-in-flight divergence for one model-turn pool.",
    },
    ModelTurnSeriesSpecV1 {
        metric: MODEL_TURN_AGGREGATE_OUTPUT_RATE,
        discriminator: None,
        discriminator_values: &[],
        description: "Aggregate output units per second across one model-turn pool's window.",
    },
    ModelTurnSeriesSpecV1 {
        metric: MODEL_TURN_STREAM_OUTPUT_RATE,
        discriminator: None,
        discriminator_values: &[],
        description: "Output units per second for one settled model-turn stream.",
    },
    ModelTurnSeriesSpecV1 {
        metric: MODEL_TURN_TTFT_SECONDS,
        discriminator: None,
        discriminator_values: &[],
        description: "Seconds from a model-turn attempt starting to its first emitted token.",
    },
    ModelTurnSeriesSpecV1 {
        metric: MODEL_TURN_THROTTLES_TOTAL,
        discriminator: Some(LABEL_BUCKET),
        discriminator_values: &["request", "input", "output", "combined"],
        description: "Model-turn admissions deferred by a bucket, by fixed bucket kind only.",
    },
    ModelTurnSeriesSpecV1 {
        metric: MODEL_TURN_EXPIRY_OUTCOMES_TOTAL,
        discriminator: Some(LABEL_OUTCOME),
        discriminator_values: &["refunded", "quarantined"],
        description: "Model-turn watchdog expiries, by fixed accounting disposition only.",
    },
    ModelTurnSeriesSpecV1 {
        metric: MODEL_TURN_IDENTITY_ELIGIBILITY,
        discriminator: None,
        discriminator_values: &[],
        description: "Whether one model-turn pool's durable identity is eligible: 1 or 0.",
    },
    ModelTurnSeriesSpecV1 {
        metric: MODEL_TURN_PROTOCOL_COVERAGE,
        discriminator: None,
        discriminator_values: &[],
        description: "Whether one model-turn pool holds complete protocol coverage: 1 or 0.",
    },
];

/// Register a description for every model-turn series.
pub fn register_metrics() {
    for spec in MODEL_TURN_SERIES {
        if spec.metric.ends_with("_total") {
            describe_counter!(spec.metric, spec.description);
        } else if spec.metric == MODEL_TURN_TTFT_SECONDS
            || spec.metric == MODEL_TURN_STREAM_OUTPUT_RATE
        {
            describe_histogram!(spec.metric, spec.description);
        } else {
            describe_gauge!(spec.metric, spec.description);
        }
    }
}

// ── Route labels, closed by construction ───────────────────────────────────

/// The active model catalog, as this crate needs to see it.
///
/// `djinn-telemetry` sits below `djinn-provider` in the dependency graph, so
/// the catalog arrives as a capability rather than a type. The point is the
/// same either way: a caller that cannot resolve the pair cannot build labels
/// for it.
pub trait ModelTurnCatalogV1 {
    /// Does the active catalog hold exactly this provider/model pair?
    fn resolves(&self, provider_id: &str, model_id: &str) -> bool;
}

/// The three route labels every model-turn series carries.
///
/// There is no public constructor that skips the catalog check, so a
/// provider/model value outside the active catalog is rejected *before*
/// emission rather than filtered afterwards.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTurnRouteLabels {
    pool_id: i64,
    provider: String,
    model: String,
}

impl ModelTurnRouteLabels {
    /// Build labels for a pool whose route the active catalog resolves.
    ///
    /// Returns `None` for a non-positive `pool_id` or a pair the catalog does
    /// not hold, and `None` means *no emission at all* rather than a redacted
    /// one.
    #[must_use]
    pub fn qualify(
        pool_id: i64,
        provider_id: &str,
        model_id: &str,
        catalog: &impl ModelTurnCatalogV1,
    ) -> Option<Self> {
        if pool_id <= 0 || !catalog.resolves(provider_id, model_id) {
            return None;
        }
        Some(Self {
            pool_id,
            provider: provider_id.to_owned(),
            model: model_id.to_owned(),
        })
    }

    #[must_use]
    pub fn pool_id(&self) -> i64 {
        self.pool_id
    }

    fn pairs(&self) -> [(&'static str, String); 3] {
        [
            (LABEL_POOL_ID, self.pool_id.to_string()),
            (LABEL_PROVIDER, self.provider.clone()),
            (LABEL_MODEL, self.model.clone()),
        ]
    }
}

/// The bucket kinds a throttle can be attributed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelTurnThrottleBucketV1 {
    Request,
    Input,
    Output,
    Combined,
}

impl ModelTurnThrottleBucketV1 {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Input => "input",
            Self::Output => "output",
            Self::Combined => "combined",
        }
    }
}

/// How a watchdog expiry disposed of the lease's reservation accounting.
///
/// A lease that never reached the provider is `Refunded`; one that may have
/// been sent is `Quarantined` until authoritative usage arrives. Those are the
/// only two dispositions [`ModelTurnAdmissionRepository::expire_lease`] can
/// write, so the value set is closed by the storage rather than by convention.
///
/// A *fenced* observation is deliberately not a value here: it means the lease
/// heartbeat between the read and the compare-and-swap, i.e. it is healthy and
/// nothing was expired at all. Counting it as an expiry outcome would report a
/// living lease as a watchdog result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelTurnExpiryOutcomeV1 {
    Refunded,
    Quarantined,
}

impl ModelTurnExpiryOutcomeV1 {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Refunded => "refunded",
            Self::Quarantined => "quarantined",
        }
    }
}

// ── Emitters ───────────────────────────────────────────────────────────────

macro_rules! route_gauge {
    ($metric:expr, $route:expr, $value:expr) => {{
        let [pool, provider, model] = $route.pairs();
        gauge!($metric, pool.0 => pool.1, provider.0 => provider.1, model.0 => model.1)
            .set($value);
    }};
}

macro_rules! route_histogram {
    ($metric:expr, $route:expr, $value:expr) => {{
        let [pool, provider, model] = $route.pairs();
        histogram!($metric, pool.0 => pool.1, provider.0 => provider.1, model.0 => model.1)
            .record($value);
    }};
}

pub fn record_pool_target(route: &ModelTurnRouteLabels, target: i64) {
    route_gauge!(MODEL_TURN_POOL_TARGET, route, target as f64);
}

pub fn record_in_flight(route: &ModelTurnRouteLabels, in_flight: i64) {
    route_gauge!(MODEL_TURN_IN_FLIGHT, route, in_flight as f64);
}

/// Reserved minus in-flight. A non-zero value means the reservation ledger and
/// the pool counter disagree.
pub fn record_reservation_divergence(route: &ModelTurnRouteLabels, divergence: i64) {
    route_gauge!(MODEL_TURN_RESERVATION_DIVERGENCE, route, divergence as f64);
}

pub fn record_aggregate_output_rate(route: &ModelTurnRouteLabels, units_per_second: f64) {
    route_gauge!(MODEL_TURN_AGGREGATE_OUTPUT_RATE, route, units_per_second);
}

pub fn record_stream_output_rate(route: &ModelTurnRouteLabels, units_per_second: f64) {
    route_histogram!(MODEL_TURN_STREAM_OUTPUT_RATE, route, units_per_second);
}

pub fn record_time_to_first_token(route: &ModelTurnRouteLabels, seconds: f64) {
    route_histogram!(MODEL_TURN_TTFT_SECONDS, route, seconds);
}

pub fn record_identity_eligibility(route: &ModelTurnRouteLabels, eligible: bool) {
    route_gauge!(
        MODEL_TURN_IDENTITY_ELIGIBILITY,
        route,
        if eligible { 1.0 } else { 0.0 }
    );
}

pub fn record_protocol_coverage(route: &ModelTurnRouteLabels, covered: bool) {
    route_gauge!(
        MODEL_TURN_PROTOCOL_COVERAGE,
        route,
        if covered { 1.0 } else { 0.0 }
    );
}

pub fn record_throttle(route: &ModelTurnRouteLabels, bucket: ModelTurnThrottleBucketV1) {
    let [pool, provider, model] = route.pairs();
    counter!(
        MODEL_TURN_THROTTLES_TOTAL,
        pool.0 => pool.1,
        provider.0 => provider.1,
        model.0 => model.1,
        LABEL_BUCKET => bucket.code(),
    )
    .increment(1);
}

pub fn record_expiry_outcome(route: &ModelTurnRouteLabels, outcome: ModelTurnExpiryOutcomeV1) {
    let [pool, provider, model] = route.pairs();
    counter!(
        MODEL_TURN_EXPIRY_OUTCOMES_TOTAL,
        pool.0 => pool.1,
        provider.0 => provider.1,
        model.0 => model.1,
        LABEL_OUTCOME => outcome.code(),
    )
    .increment(1);
}

// ── Capture support ────────────────────────────────────────────────────────

/// Every `(metric, label_key, label_value)` triple present in a rendered
/// Prometheus exposition, restricted to model-turn series.
///
/// Parsing the rendered registry is what makes the assertion about what was
/// *emitted* rather than about what the emitters were asked to emit.
#[must_use]
pub fn model_turn_label_triples(
    rendered: &str,
) -> std::collections::BTreeSet<(String, String, String)> {
    let mut triples = std::collections::BTreeSet::new();
    for line in rendered.lines() {
        if line.starts_with('#') || !line.starts_with("djinn_model_turn_") {
            continue;
        }
        let Some((series, _)) = line.split_once(' ') else {
            continue;
        };
        let (metric, labels) = match series.split_once('{') {
            Some((metric, rest)) => (metric, rest.trim_end_matches('}')),
            None => (series, ""),
        };
        // Histograms render as `<metric>_bucket`/`_sum`/`_count`; attribute
        // every one of them to the series it belongs to.
        let metric = MODEL_TURN_SERIES
            .iter()
            .find(|spec| metric == spec.metric || metric.starts_with(&format!("{}_", spec.metric)))
            .map_or(metric, |spec| spec.metric);
        for pair in labels.split(',').filter(|pair| !pair.is_empty()) {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            // `le` and `quantile` are added by the exposition format itself
            // when a histogram renders; they are not labels any emitter here
            // supplies, and they carry a bucket boundary, never an identity.
            if key == "le" || key == "quantile" {
                continue;
            }
            triples.insert((
                metric.to_owned(),
                key.to_owned(),
                value.trim_matches('"').to_owned(),
            ));
        }
    }
    triples
}

/// Expand [`MODEL_TURN_SERIES`] into the exact triple set a fixture with these
/// route labels must produce when every series is emitted once.
#[must_use]
pub fn expected_label_triples(
    pool_id: i64,
    provider: &str,
    model: &str,
) -> std::collections::BTreeSet<(String, String, String)> {
    let mut expected = std::collections::BTreeSet::new();
    for spec in MODEL_TURN_SERIES {
        for (key, value) in [
            (LABEL_POOL_ID, pool_id.to_string()),
            (LABEL_PROVIDER, provider.to_owned()),
            (LABEL_MODEL, model.to_owned()),
        ] {
            expected.insert((spec.metric.to_owned(), key.to_owned(), value));
        }
        if let Some(discriminator) = spec.discriminator {
            for value in spec.discriminator_values {
                expected.insert((
                    spec.metric.to_owned(),
                    discriminator.to_owned(),
                    (*value).to_owned(),
                ));
            }
        }
    }
    expected
}

#[cfg(test)]
#[path = "model_turn_metrics_tests.rs"]
mod tests;
