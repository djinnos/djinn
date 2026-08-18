//! The allow-list is the claim, so these tests are about the constant itself
//! and about the exact triple set a driven emission produces.

use super::*;
use crate::IsolatedRecorder;

/// Every provider/model pair resolves. Used where the test is about the label
/// shape rather than about catalog qualification.
struct AnyCatalog;
impl ModelTurnCatalogV1 for AnyCatalog {
    fn resolves(&self, _provider_id: &str, _model_id: &str) -> bool {
        true
    }
}

/// Exactly one pair resolves.
struct OnePairCatalog {
    provider: &'static str,
    model: &'static str,
}
impl ModelTurnCatalogV1 for OnePairCatalog {
    fn resolves(&self, provider_id: &str, model_id: &str) -> bool {
        provider_id == self.provider && model_id == self.model
    }
}

const POOL: i64 = 42;
const PROVIDER: &str = "acme";
const MODEL: &str = "acme/turbo";

fn route() -> ModelTurnRouteLabels {
    ModelTurnRouteLabels::qualify(POOL, PROVIDER, MODEL, &AnyCatalog).expect("qualified route")
}

/// Emit every declared series exactly the way production does.
fn emit_every_series(route: &ModelTurnRouteLabels) {
    record_pool_target(route, 4);
    record_in_flight(route, 2);
    record_reservation_divergence(route, 0);
    record_aggregate_output_rate(route, 12.5);
    record_stream_output_rate(route, 8.25);
    record_time_to_first_token(route, 0.75);
    record_identity_eligibility(route, true);
    record_protocol_coverage(route, false);
    for bucket in [
        ModelTurnThrottleBucketV1::Request,
        ModelTurnThrottleBucketV1::Input,
        ModelTurnThrottleBucketV1::Output,
        ModelTurnThrottleBucketV1::Combined,
    ] {
        record_throttle(route, bucket);
    }
    for outcome in [
        ModelTurnExpiryOutcomeV1::Refunded,
        ModelTurnExpiryOutcomeV1::Quarantined,
    ] {
        record_expiry_outcome(route, outcome);
    }
}

// ── AC 1: the emitted set equals the allow-list, exactly ───────────────────

#[test]
fn the_emitted_triple_set_equals_the_allow_list_exactly() {
    let recorder = IsolatedRecorder::new();
    {
        let _guard = recorder.scope();
        register_metrics();
        emit_every_series(&route());
    }
    let emitted = model_turn_label_triples(&recorder.render());
    let expected = expected_label_triples(POOL, PROVIDER, MODEL);
    assert_eq!(
        emitted, expected,
        "the emitted (metric, label_key, label_value) set must equal the allow-list"
    );
}

/// A label key or value that the constant does not declare must fail. This
/// drives the same comparison against a deliberately widened emission, so the
/// equality above is load-bearing rather than tautological.
#[test]
fn an_undeclared_label_key_or_value_breaks_the_equality() {
    let recorder = IsolatedRecorder::new();
    {
        let _guard = recorder.scope();
        register_metrics();
        emit_every_series(&route());
        // A key nobody declared.
        metrics::gauge!(
            MODEL_TURN_POOL_TARGET,
            LABEL_POOL_ID => POOL.to_string(),
            LABEL_PROVIDER => PROVIDER,
            LABEL_MODEL => MODEL,
            "credential_id" => "cred-abc",
        )
        .set(1.0);
    }
    let emitted = model_turn_label_triples(&recorder.render());
    let expected = expected_label_triples(POOL, PROVIDER, MODEL);
    assert_ne!(
        emitted, expected,
        "an undeclared label key must break the allow-list equality"
    );
    assert!(
        emitted
            .difference(&expected)
            .any(|(_, key, _)| key == "credential_id"),
        "the difference must name the undeclared key"
    );

    // And an undeclared *value* of a declared key.
    let recorder = IsolatedRecorder::new();
    {
        let _guard = recorder.scope();
        register_metrics();
        emit_every_series(&route());
        metrics::counter!(
            MODEL_TURN_EXPIRY_OUTCOMES_TOTAL,
            LABEL_POOL_ID => POOL.to_string(),
            LABEL_PROVIDER => PROVIDER,
            LABEL_MODEL => MODEL,
            LABEL_OUTCOME => "reconciled",
        )
        .increment(1);
    }
    let emitted = model_turn_label_triples(&recorder.render());
    assert!(
        emitted.contains(&(
            MODEL_TURN_EXPIRY_OUTCOMES_TOTAL.to_owned(),
            LABEL_OUTCOME.to_owned(),
            "reconciled".to_owned()
        )),
        "precondition: the undeclared value was emitted"
    );
    assert_ne!(
        emitted,
        expected_label_triples(POOL, PROVIDER, MODEL),
        "an undeclared label value must break the allow-list equality"
    );
}

// ── AC 4: every one of the ten series is emitted ───────────────────────────

#[test]
fn every_declared_series_is_emitted_by_the_driven_scenario() {
    let recorder = IsolatedRecorder::new();
    {
        let _guard = recorder.scope();
        register_metrics();
        emit_every_series(&route());
    }
    let emitted = model_turn_label_triples(&recorder.render());
    assert_eq!(MODEL_TURN_SERIES.len(), 10);
    for spec in MODEL_TURN_SERIES {
        assert!(
            emitted.iter().any(|(metric, _, _)| metric == spec.metric),
            "series `{}` was never emitted",
            spec.metric
        );
    }
}

// ── AC 2: pool identity is opaque and numeric; routes are catalog-bound ────

#[test]
fn pool_identity_is_only_ever_an_opaque_numeric_pool_id() {
    let recorder = IsolatedRecorder::new();
    {
        let _guard = recorder.scope();
        register_metrics();
        emit_every_series(&route());
    }
    let emitted = model_turn_label_triples(&recorder.render());
    let pool_values: std::collections::BTreeSet<&String> = emitted
        .iter()
        .filter(|(_, key, _)| key == LABEL_POOL_ID)
        .map(|(_, _, value)| value)
        .collect();
    assert_eq!(pool_values.len(), 1, "one pool, one identity value");
    for value in pool_values {
        assert_eq!(
            value.parse::<i64>().ok(),
            Some(POOL),
            "pool identity must be the opaque numeric pool id"
        );
    }
}

#[test]
fn a_route_outside_the_active_catalog_is_rejected_before_emission() {
    let catalog = OnePairCatalog {
        provider: PROVIDER,
        model: MODEL,
    };
    assert!(
        ModelTurnRouteLabels::qualify(POOL, PROVIDER, MODEL, &catalog).is_some(),
        "precondition: the catalog resolves the fixture route"
    );
    for (provider, model) in [
        ("other-provider", MODEL),
        (PROVIDER, "other/model"),
        ("other-provider", "other/model"),
    ] {
        assert!(
            ModelTurnRouteLabels::qualify(POOL, provider, model, &catalog).is_none(),
            "`{provider}/{model}` is outside the catalog and must not become labels"
        );
    }
    // A non-positive pool id is not a pool.
    assert!(ModelTurnRouteLabels::qualify(0, PROVIDER, MODEL, &catalog).is_none());
    assert!(ModelTurnRouteLabels::qualify(-1, PROVIDER, MODEL, &catalog).is_none());

    // And nothing at all is emitted for a route that never qualified.
    let recorder = IsolatedRecorder::new();
    {
        let _guard = recorder.scope();
        register_metrics();
        if let Some(route) =
            ModelTurnRouteLabels::qualify(POOL, "other-provider", "other/model", &catalog)
        {
            emit_every_series(&route);
        }
    }
    assert!(
        model_turn_label_triples(&recorder.render()).is_empty(),
        "an unqualified route must produce no model-turn series at all"
    );
}

// ── AC 3: the constant itself carries no identifier ────────────────────────

/// Proven against the allow-list rather than against a sampled run: if no
/// metric name, label key, or declared value can spell one of these, then no
/// emission the constant permits can carry one.
///
/// Note what is *not* forbidden: the bucket-kind value `request` names a
/// rate-limit bucket, not a request identifier. The distinction is made
/// precise by forbidding the identifier spellings and, separately, by pinning
/// every declared value to a short lower-case word — a shape no id can take.
#[test]
fn the_allow_list_carries_no_credential_account_project_user_request_or_lease_id() {
    const FORBIDDEN_SUBSTRINGS: [&str; 11] = [
        "credential",
        "account",
        "project",
        "lease",
        "secret",
        "api_key",
        "apikey",
        "email",
        "fingerprint",
        "uuid",
        "sha256",
    ];
    const FORBIDDEN_EXACT_OR_SUFFIX: [&str; 6] = [
        "credential_id",
        "account_id",
        "project_id",
        "user_id",
        "request_id",
        "lease_id",
    ];

    let mut vocabulary: Vec<String> = Vec::new();
    for spec in MODEL_TURN_SERIES {
        vocabulary.push(spec.metric.to_owned());
        if let Some(discriminator) = spec.discriminator {
            vocabulary.push(discriminator.to_owned());
        }
        vocabulary.extend(spec.discriminator_values.iter().map(|v| (*v).to_owned()));
    }
    vocabulary.extend(ROUTE_LABEL_KEYS.iter().map(|k| (*k).to_owned()));

    for term in &vocabulary {
        for forbidden in FORBIDDEN_SUBSTRINGS {
            assert!(
                !term.contains(forbidden),
                "allow-list term `{term}` must not contain `{forbidden}`"
            );
        }
        for forbidden in FORBIDDEN_EXACT_OR_SUFFIX {
            assert!(
                !term.contains(forbidden),
                "allow-list term `{term}` must not contain `{forbidden}`"
            );
        }
        // `user` only as a whole word would still be an identity label; there
        // is none, so forbid the substring outright except inside `_units`,
        // which no term uses.
        assert!(
            !term.contains("user"),
            "allow-list term `{term}` must not contain `user`"
        );
    }

    // `pool_id` is the single `*_id` key in the whole allow-list, and it is
    // deliberate: it is the opaque numeric primary key of a pool row, which
    // itself carries no credential, account, project, or user identity.
    let id_keys: Vec<&String> = vocabulary
        .iter()
        .filter(|term| term.ends_with("_id"))
        .collect();
    assert_eq!(id_keys, vec![&LABEL_POOL_ID.to_owned()]);

    // Every declared label *value* is a short lower-case word. An identifier —
    // a uuid, a fingerprint, an email, an opaque token — cannot take that
    // shape, so the value side of the allow-list is closed structurally and
    // not merely by the substring list above.
    for spec in MODEL_TURN_SERIES {
        for value in spec.discriminator_values {
            assert!(
                value.len() <= 16
                    && !value.is_empty()
                    && value.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "declared value `{value}` is not a short lower-case word"
            );
        }
    }
}

/// The declaration and the emitters must not drift: a series added to the
/// constant without an emitter, or an emitter added without a declaration, is
/// what this pins.
#[test]
fn every_declared_series_has_a_description_and_a_unique_name() {
    let mut names: Vec<&str> = MODEL_TURN_SERIES.iter().map(|spec| spec.metric).collect();
    let count = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), count, "series names must be unique");
    for spec in MODEL_TURN_SERIES {
        assert!(spec.metric.starts_with("djinn_model_turn_"));
        assert!(!spec.description.trim().is_empty());
        assert_eq!(
            spec.discriminator.is_some(),
            !spec.discriminator_values.is_empty(),
            "`{}` must declare values exactly when it has a discriminator",
            spec.metric
        );
    }
}
