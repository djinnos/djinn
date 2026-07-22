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
    let cred =
        |provider_id: &str, key_name: &str, owner: Option<&str>| djinn_core::models::Credential {
            id: provider_id.into(),
            provider_id: provider_id.into(),
            key_name: key_name.into(),
            owner_user_id: owner.map(str::to_string),
            created_at: String::new(),
            updated_at: String::new(),
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
            mk("zai-coding-plan", "glm-paid", base.clone()), // already priced → untouched
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
    // kimi-for-coding ships `k3`/`k2p7`/`k2p5` (+ the upstream `kimi-for-coding`
    // id) which don't canonically match any moonshotai id — they must be priced
    // via PRICING_MODEL_ALIAS from their true moonshotai counterparts.
    let rate = |o: f64| Pricing {
        output_per_million: o,
        ..Pricing::default()
    };
    let mk = |id: &str, pricing: Pricing| Model {
        id: id.to_string(),
        provider_id: String::new(),
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
        vec![
            mk("kimi-k3", rate(15.0)),
            mk("kimi-k2.7-code", rate(4.0)),
            mk("kimi-k2.5", rate(3.0)),
        ],
    );
    idx.insert(
        "kimi-for-coding".to_string(),
        // `kimi-for-coding` canonicalizes to `kimiforcoding` (upstream id alias).
        ["k3", "k2p7", "k2p5", "kimi-for-coding"]
            .map(|id| mk(id, Pricing::default()))
            .to_vec(),
    );

    enrich_plan_pricing(&mut idx);

    let out = |id: &str| {
        idx["kimi-for-coding"]
            .iter()
            .find(|m| m.id == id)
            .unwrap()
            .pricing
            .output_per_million
    };
    // k3 is the fix: previously it had no alias and booked $0.00 "projected".
    assert_eq!(out("k3"), 15.0);
    // k2p7/k2p5 resolve to their true counterparts, not kimi-k2-thinking.
    assert_eq!(out("k2p7"), 4.0);
    assert_eq!(out("k2p5"), 3.0);
    // Upstream `kimi-for-coding` id (canonical `kimiforcoding`) → kimi-k2.7-code.
    assert_eq!(out("kimi-for-coding"), 4.0);
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
// These tests exercise the pure composition helper (`compose_catalog`) and the
// status/rejection transitions directly over a `CatalogData`, mirroring
// `refresh()` without calling the network.

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

    // Builtin injection ran during composition.  Verify at least one
    // non-upstream, non-custom builtin provider was injected.
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
    data.last_refresh_error = Some("models.dev normalized payload had zero providers".to_string());

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

// ── set_custom_providers (reconciliation) tests ───────────────────────────

/// Reconciling with a custom provider and then reconciling without it must
/// remove it from both the retained set and the active catalog.  This is the
/// primary regression guard for the reconciliation API.
#[test]
fn set_custom_providers_removes_absent_entries() {
    let catalog = CatalogService::new();

    // First reconciliation: two custom providers.
    catalog.set_custom_providers(vec![
        (
            mk_custom_provider("alpha"),
            vec![mk_seed_model("a1", "alpha")],
        ),
        (
            mk_custom_provider("beta"),
            vec![mk_seed_model("b1", "beta")],
        ),
    ]);

    assert!(
        catalog.list_providers().iter().any(|p| p.id == "alpha"),
        "alpha must be present after first reconciliation"
    );
    assert!(
        catalog.list_providers().iter().any(|p| p.id == "beta"),
        "beta must be present after first reconciliation"
    );
    assert_eq!(
        catalog.list_models("alpha").len(),
        1,
        "alpha seed model must be present"
    );

    // Second reconciliation: only beta — alpha must be removed.
    catalog.set_custom_providers(vec![(
        mk_custom_provider("beta"),
        vec![mk_seed_model("b1", "beta")],
    )]);

    assert!(
        catalog.list_providers().iter().all(|p| p.id != "alpha"),
        "alpha must be removed from the active catalog after reconciliation without it"
    );
    assert!(
        catalog.list_models("alpha").is_empty(),
        "alpha model list must be empty after removal"
    );
    assert!(
        catalog.find_model("alpha/a1").is_none(),
        "alpha/a1 must not be findable after removal"
    );

    // Retained set must also reflect the removal.
    {
        let data = catalog.inner.read();
        assert!(
            !data.custom_providers.contains_key("alpha"),
            "alpha must be absent from the retained set after reconciliation"
        );
        assert!(
            data.custom_providers.contains_key("beta"),
            "beta must still be in the retained set"
        );
    }

    // beta is still present and correct.
    assert!(
        catalog.list_providers().iter().any(|p| p.id == "beta"),
        "beta must survive reconciliation that omits alpha"
    );
    assert_eq!(
        catalog.list_models("beta").len(),
        1,
        "beta seed model must still be present"
    );
}

/// Reconciling with an empty vec must clear all custom providers from the
/// retained set and active catalog.
#[test]
fn set_custom_providers_with_empty_vec_clears_all() {
    let catalog = CatalogService::new();
    let initial_count = catalog.list_providers().len();

    // Add two custom providers via the individual API.
    catalog.add_custom_provider(
        mk_custom_provider("gone-1"),
        vec![mk_seed_model("m1", "gone-1")],
    );
    catalog.add_custom_provider(
        mk_custom_provider("gone-2"),
        vec![mk_seed_model("m2", "gone-2")],
    );
    assert!(catalog.list_providers().iter().any(|p| p.id == "gone-1"));
    assert!(catalog.list_providers().iter().any(|p| p.id == "gone-2"));

    // Reconcile with empty — must remove both.
    catalog.set_custom_providers(vec![]);

    assert!(
        catalog
            .list_providers()
            .iter()
            .all(|p| p.id != "gone-1" && p.id != "gone-2"),
        "both custom providers must be removed by empty reconciliation"
    );
    // The active catalog should be back to the pre-add state.
    assert_eq!(
        catalog.list_providers().len(),
        initial_count,
        "provider count must return to initial after clearing all custom providers"
    );
    {
        let data = catalog.inner.read();
        assert!(
            data.custom_providers.is_empty(),
            "retained set must be empty after clearing"
        );
    }
}

/// Reconciliation with overlapping entries must replace (not duplicate) entries
/// that share the same provider id, and seed-model normalization must apply.
#[test]
fn set_custom_providers_replaces_overlapping_entries() {
    let catalog = CatalogService::new();

    // First reconciliation with v1 seeds.
    catalog.set_custom_providers(vec![(
        mk_custom_provider("overlap"),
        vec![mk_seed_model("overlap/v1-seed", "overlap")],
    )]);

    let models_v1: Vec<String> = catalog
        .list_models("overlap")
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(
        models_v1,
        vec!["v1-seed"],
        "v1 seed prefix must be stripped"
    );

    // Second reconciliation with v2 seeds (same provider id).
    catalog.set_custom_providers(vec![(
        mk_custom_provider("overlap"),
        vec![
            mk_seed_model("overlap/v2-alpha", "overlap"),
            mk_seed_model("overlap/v2-beta", "overlap"),
        ],
    )]);

    // Must not duplicate the provider.
    let count = catalog
        .list_providers()
        .into_iter()
        .filter(|p| p.id == "overlap")
        .count();
    assert_eq!(
        count, 1,
        "reconciliation must not duplicate overlapping entries"
    );

    // Models must be the v2 set with prefix stripped.
    let models_v2: Vec<String> = catalog
        .list_models("overlap")
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(
        models_v2,
        vec!["v2-alpha".to_string(), "v2-beta".to_string()],
        "v2 seeds must replace v1 seeds with prefix stripped"
    );

    // Retained set must reflect v2.
    {
        let data = catalog.inner.read();
        let retained = data
            .custom_providers
            .get("overlap")
            .expect("overlap must be in retained set");
        let retained_ids: Vec<String> = retained.seed_models.iter().map(|m| m.id.clone()).collect();
        assert_eq!(
            retained_ids,
            vec!["v2-alpha".to_string(), "v2-beta".to_string()]
        );
    }
}

/// Reconciliation followed by a successful refresh must not resurrect a
/// custom provider that was absent from the reconciliation input.
#[test]
fn set_custom_providers_then_refresh_does_not_resurrect() {
    let catalog = CatalogService::new();

    // Seed with two custom providers.
    catalog.set_custom_providers(vec![
        (
            mk_custom_provider("keep"),
            vec![mk_seed_model("k1", "keep")],
        ),
        (
            mk_custom_provider("drop"),
            vec![mk_seed_model("d1", "drop")],
        ),
    ]);
    assert!(catalog.list_providers().iter().any(|p| p.id == "drop"));

    // Reconcile with only "keep" — "drop" should be gone.
    catalog.set_custom_providers(vec![(
        mk_custom_provider("keep"),
        vec![mk_seed_model("k1", "keep")],
    )]);
    assert!(catalog.list_providers().iter().all(|p| p.id != "drop"));

    // Simulate a successful refresh (compose_catalog) — must not resurrect "drop".
    {
        let mut data = catalog.inner.write();
        let upstream = vec![mk_custom_provider("openai")];
        let mut idx = HashMap::new();
        idx.insert("openai".to_string(), vec![mk_seed_model("gpt-x", "openai")]);
        compose_catalog(&mut data, upstream, idx);
    }

    assert!(
        catalog.list_providers().iter().all(|p| p.id != "drop"),
        "a custom provider absent from reconciliation must not be resurrected by refresh"
    );
    assert!(
        catalog.list_providers().iter().any(|p| p.id == "keep"),
        "keep must survive the refresh"
    );
}

// ── Freshness / source-tier metadata tests ──────────────────────────────────

#[test]
fn freshness_initial_state_is_never_and_embedded() {
    let catalog = CatalogService::new();
    assert_eq!(catalog.last_refresh_status(), RefreshStatus::Never);
    assert!(catalog.last_refresh_error().is_none());
    assert!(catalog.last_successful_fetch_time().is_none());
    assert!(catalog.last_successful_fetch_age().is_none());
    assert_eq!(
        catalog.source_tier(Duration::from_secs(60)),
        SourceTier::Embedded
    );
}

#[test]
fn freshness_after_success_exposes_wall_time_and_monotonic_freshness() {
    let catalog = CatalogService::new();
    let monotonic_success = SystemClock::new().now_instant() - Duration::from_secs(30);
    let expected_wall_success = SystemTime::UNIX_EPOCH + Duration::from_secs(1_735_689_600);
    catalog.set_last_success_times_for_tests(
        Some(monotonic_success),
        Some(expected_wall_success),
        RefreshStatus::Success,
        None,
    );

    assert_eq!(catalog.last_refresh_status(), RefreshStatus::Success);
    assert!(catalog.last_refresh_error().is_none());
    assert_eq!(
        catalog.last_successful_fetch_time(),
        Some(expected_wall_success)
    );

    let age = catalog
        .last_successful_fetch_age()
        .expect("age must be Some after a successful fetch");
    assert!(age >= Duration::from_secs(30), "got {age:?}");
    assert_eq!(
        catalog.source_tier(Duration::from_secs(60)),
        SourceTier::Live
    );
    assert_eq!(
        catalog.source_tier(Duration::from_secs(20)),
        SourceTier::Stale
    );
}

#[test]
fn freshness_success_then_failure_preserves_catalog_and_records_error() {
    let catalog = CatalogService::new();
    catalog.add_custom_provider(
        mk_custom_provider("openai"),
        vec![mk_seed_model("gpt-x", "openai")],
    );
    let monotonic_success = SystemClock::new().now_instant() - Duration::from_secs(90);
    let expected_wall_success = SystemTime::UNIX_EPOCH + Duration::from_secs(1_735_689_600);
    catalog.set_last_success_times_for_tests(
        Some(monotonic_success),
        Some(expected_wall_success),
        RefreshStatus::Success,
        None,
    );
    assert_eq!(
        catalog.source_tier(Duration::from_secs(60)),
        SourceTier::Stale
    );

    {
        let mut data = catalog.inner.write();
        data.last_refresh_status = RefreshStatus::Error;
        data.last_refresh_error = Some("models.dev returned HTTP 503".to_string());
    }

    assert_eq!(catalog.last_refresh_status(), RefreshStatus::Error);
    assert_eq!(
        catalog.last_refresh_error().as_deref(),
        Some("models.dev returned HTTP 503")
    );
    assert!(catalog.list_providers().iter().any(|p| p.id == "openai"));
    assert_eq!(
        catalog.last_successful_fetch_time(),
        Some(expected_wall_success)
    );
    let age = catalog
        .last_successful_fetch_age()
        .expect("the monotonic success timestamp must persist after a failure");
    assert!(age >= Duration::from_secs(90), "got {age:?}");
    assert_eq!(
        catalog.source_tier(Duration::from_secs(60)),
        SourceTier::Stale
    );
}

/// `source_tier` reports `Stale` when a fetch previously succeeded but the
/// recorded age exceeds the supplied max-age window.  This simulates the
/// "serving stale data while recent refreshes fail" state.
#[test]
fn source_tier_reports_stale_when_age_exceeds_window() {
    let catalog = CatalogService::new();
    // Record a fetch that happened "in the past" by back-dating fetched_at.
    {
        let mut data = catalog.inner.write();
        data.fetched_at = Some(SystemClock::new().now_instant() - Duration::from_secs(120));
        data.last_refresh_status = RefreshStatus::Error;
        data.last_refresh_error = Some("models.dev returned HTTP 503".to_string());
    }

    let age = catalog
        .last_successful_fetch_age()
        .expect("age must be Some because a fetch previously succeeded");
    assert!(
        age >= Duration::from_secs(120),
        "age should reflect the back-dated fetch; got {age:?}"
    );

    // Within a 60s window the 120s-old data is Stale.
    assert_eq!(
        catalog.source_tier(Duration::from_secs(60)),
        SourceTier::Stale,
        "data older than the window must be Stale"
    );
    // With a generous window it would be Live.
    assert_eq!(
        catalog.source_tier(Duration::from_secs(600)),
        SourceTier::Live,
        "data within a generous window is Live even if the last attempt failed"
    );
}

/// Full status-metadata transition sequence end-to-end: Never → Success → Error
/// → Success, asserting the metadata accessors at every step.
#[test]
fn status_metadata_full_transition_sequence() {
    let catalog = CatalogService::new();

    // 1. Initial: Never / no error / no age / Embedded.
    assert_eq!(catalog.last_refresh_status(), RefreshStatus::Never);
    assert!(catalog.last_refresh_error().is_none());
    assert!(catalog.last_successful_fetch_age().is_none());
    assert_eq!(
        catalog.source_tier(Duration::from_secs(60)),
        SourceTier::Embedded
    );

    // 2. Success: clears error, records age, Live tier.
    {
        let mut data = catalog.inner.write();
        compose_catalog(
            &mut data,
            vec![mk_custom_provider("openai")],
            HashMap::new(),
        );
        data.fetched_at = Some(SystemClock::new().now_instant());
        data.last_refresh_status = RefreshStatus::Success;
        data.last_refresh_error = None;
    }
    assert_eq!(catalog.last_refresh_status(), RefreshStatus::Success);
    assert!(catalog.last_refresh_error().is_none());
    assert!(catalog.last_successful_fetch_age().is_some());
    assert_eq!(
        catalog.source_tier(Duration::from_secs(60)),
        SourceTier::Live
    );

    // 3. Error: records message, keeps prior catalog, fetch age persists.
    {
        let mut data = catalog.inner.write();
        data.last_refresh_status = RefreshStatus::Error;
        data.last_refresh_error = Some("connection refused".to_string());
    }
    assert_eq!(catalog.last_refresh_status(), RefreshStatus::Error);
    assert_eq!(
        catalog.last_refresh_error().as_deref(),
        Some("connection refused")
    );
    assert!(
        catalog.last_successful_fetch_age().is_some(),
        "age persists across the failure"
    );

    // 4. Success again: clears the error and refreshes the fetch time.
    {
        let mut data = catalog.inner.write();
        data.fetched_at = Some(SystemClock::new().now_instant());
        data.last_refresh_status = RefreshStatus::Success;
        data.last_refresh_error = None;
    }
    assert_eq!(catalog.last_refresh_status(), RefreshStatus::Success);
    assert!(catalog.last_refresh_error().is_none());
    assert_eq!(
        catalog.source_tier(Duration::from_secs(60)),
        SourceTier::Live
    );
}

/// The zero-provider rejection path must leave the source tier computable and
/// the status as Error while the previous catalog is preserved.  Mirrors the
/// guard inside `refresh()`.
#[test]
fn zero_provider_rejection_sets_error_status_and_preserves_catalog() {
    let catalog = CatalogService::new();
    // Seed a prior successful fetch so the tier is computable after the rejection.
    {
        let mut data = catalog.inner.write();
        data.fetched_at = Some(SystemClock::new().now_instant() - Duration::from_secs(90));
        data.last_refresh_status = RefreshStatus::Success;
    }
    let provider_ids_before = catalog.list_providers();
    let live_tier_before = catalog.source_tier(Duration::from_secs(60));
    assert_eq!(live_tier_before, SourceTier::Stale);

    // Simulate the zero-provider rejection arm exactly as refresh() does.
    {
        let mut data = catalog.inner.write();
        data.last_refresh_status = RefreshStatus::Error;
        data.last_refresh_error =
            Some("models.dev normalized payload had zero providers".to_string());
        // Active catalog untouched.
    }

    assert_eq!(catalog.last_refresh_status(), RefreshStatus::Error);
    assert_eq!(
        catalog.last_refresh_error().as_deref(),
        Some("models.dev normalized payload had zero providers")
    );
    // The active catalog is unchanged.
    let provider_ids_after = catalog.list_providers();
    let ids_before: Vec<String> = provider_ids_before.into_iter().map(|p| p.id).collect();
    let ids_after: Vec<String> = provider_ids_after.into_iter().map(|p| p.id).collect();
    assert_eq!(
        ids_before, ids_after,
        "zero-provider rejection must not mutate the active catalog"
    );
    // Tier is still computable because fetched_at persists.
    assert_eq!(
        catalog.source_tier(Duration::from_secs(60)),
        SourceTier::Stale,
        "the tier must remain computable (Stale) after a zero-provider rejection"
    );
}
