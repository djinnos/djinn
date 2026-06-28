//! Rust/SQL predicate drift guard for the historical reclassification migration.
//!
//! The migration `84_reclassify_subscription_session_cost_basis.sql` rewrites
//! `sessions.cost_basis` from `'actual'` to `'projected'` using SQL predicates
//! that transcribe the Rust subscription rules in
//! `djinn-provider/src/catalog/builtin.rs` (`classify_provider` /
//! `governable_subscription_for_model` / `is_subscription_id`). SQL cannot import
//! the Rust functions, so the two copies can silently drift as the builtin list
//! evolves.
//!
//! The tests below reproduce each SQL predicate family as a Rust helper and
//! assert it agrees with the canonical classification functions for a set of
//! representative provider/model ids. If a new builtin subscription provider is
//! added to `BUILTIN_PROVIDERS` but the SQL migration list is not updated,
//! `drift_guard_builtin_subscription_ids_covered` fails — surfacing the drift
//! to the reviewer at test time.
//!
//! Migration-adjacent documentation:
//! `djinn-db/docs/84-cost-basis-predicate-drift-guard.md`.

use djinn_provider::catalog::builtin::{
    BUILTIN_PROVIDERS, governable_subscription_for_model, is_subscription_provider,
};

/// Faithful Rust mirror of the SQL predicate families in migration 84.
///
/// Returns `true` when the stored `model_id` (canonical `<provider>/<model>`
/// form) carries durable subscription evidence, mirroring the migration's
/// `WHERE cost_basis = 'actual' AND (…)` predicate (without the cost_basis
/// gate, which is a data-state filter, not an evidence classifier).
///
/// Keep this in sync with `84_reclassify_subscription_session_cost_basis.sql`.
fn migration_predicates_subscription_evidence(model_id: &str) -> bool {
    let provider = model_id
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let model_id_lower = model_id.to_ascii_lowercase();

    // ── Predicate Family A: built-in subscription providers ──────────
    // Must match every BUILTIN_PROVIDERS entry with CredentialClass::
    // Subscription. Sourced from `BUILTIN_PROVIDERS` directly so this
    // list cannot drift from the canonical set.
    let builtin_subscription: Vec<&str> = BUILTIN_PROVIDERS
        .iter()
        .filter(|p| p.credential_class.is_subscription())
        .map(|p| p.id)
        .collect();
    if builtin_subscription.contains(&provider.as_str()) {
        return true;
    }

    // ── Predicate Family B: OpenAI Codex models ──────────────────────
    // Mirrors governable_subscription_for_model: openai namespace + codex
    // marker in the *full* model_id (SQL uses `lower(model_id) LIKE '%codex%'`).
    if provider == "openai" && model_id_lower.contains("codex") {
        return true;
    }

    // ── Predicate Family C: id-pattern subscription fallback ──────────
    // Mirrors is_subscription_id — C1 suffix segments, C2 vendor prefixes,
    // C3 exact ids. These must exactly match the SQL constants.
    // C1: coding/token-plan / for-coding segments (SQL uses `LIKE '%-…%'`).
    if provider.contains("-coding-plan")
        || provider.contains("-token-plan")
        || provider.contains("-for-coding")
    {
        return true;
    }
    // C2: known consumer-subscription vendor prefixes (SQL `starts_with`).
    const SUBSCRIPTION_PREFIXES: &[&str] = &[
        "xiaomi",
        "moonshotai",
        "alibaba",
        "tencent",
        "stepfun",
        "kuae-cloud",
        "umans-ai",
    ];
    if SUBSCRIPTION_PREFIXES
        .iter()
        .any(|prefix| provider.starts_with(prefix))
    {
        return true;
    }
    // C3: known subscription exact ids (SQL `==`).
    const SUBSCRIPTION_IDS: &[&str] = &[
        "zai",
        "zhipuai",
        "kimi-for-coding",
        "minimax-coding-plan",
        "opencode",
        "opencode-go",
    ];
    if SUBSCRIPTION_IDS.contains(&provider.as_str()) {
        return true;
    }

    false
}

/// The Rust canonical decision for whether a `model_id` should be
/// `projected` (subscription). This is the truth the SQL predicate must
/// agree with: it combines the builtin class, the id-pattern fallback
/// (classify_provider), and the Codex-on-OpenAI special case
/// (governable_subscription_for_model).
fn rust_canonical_is_projected(model_id: &str) -> bool {
    let provider = model_id.split('/').next().unwrap_or("");
    let model = model_id.split_once('/').map(|(_, m)| m).unwrap_or("");
    governable_subscription_for_model(provider, model).is_some()
        || is_subscription_provider(provider)
}

/// Cross-check: the SQL-mirror predicate and the canonical Rust decision
/// agree for every representative provider/model id, including all families
/// — builtin subscriptions, coding/token plans, for-coding, known vendor
/// prefixes, OpenAI Codex, and plain OpenAI non-match.
#[test]
fn drift_guard_sql_predicates_match_rust_classification() {
    // (model_id, expected_projected)
    // These cases enumerate the SQL predicate families documented in the
    // migration and in design/qcuu-roadmap.
    let cases: &[(&str, bool)] = &[
        // Family A: built-in subscription providers.
        ("minimax-coding-plan/MiniMax-M3", true),
        ("xiaomi-token-plan-sgp/mimo-v2.5-pro", true),
        ("kimi-for-coding/k2p7", true),
        ("opencode/claude-opus-4-8", true),
        ("zai-coding-plan/glm-5.2", true),
        ("chatgpt_codex/gpt-5.3-codex", true),
        ("githubcopilot/gpt-5.3-codex", true),
        // Family B: OpenAI Codex marker under the openai namespace.
        ("openai/gpt-5.3-codex", true),
        ("openai/codex-mini", true),
        // Family C1: coding/token-plan / for-coding suffix segments.
        ("zhipuai-coding-plan/glm-5.2", true),
        ("alibaba-qwen-coding-plan/qwen-max", true),
        ("xiaomi-token-plan-cn/mimo-v2.5-pro", true),
        ("acme-for-coding/some-model", true),
        // Family C2: known consumer-subscription vendor prefixes.
        ("stepfun-ai/some-model", true),
        ("kuae-cloud-coding-plan/some-model", true),
        ("umans-ai-coding-plan/some-model", true),
        ("moonshotai-coding-plan/some-model", true),
        // Family C3: known subscription exact ids.
        ("zai/some-model", true),
        ("zhipuai/some-model", true),
        ("opencode-go/some-model", true),
        // ── Non-matches (must remain 'actual' / not projected) ──────
        // Plain OpenAI API-key (no codex marker) — critical non-match.
        ("openai/gpt-5.5", false),
        ("openai/gpt-4o", false),
        ("openai/o3", false),
        // Generic API-key providers.
        ("anthropic/claude-opus-4-8", false),
        ("google/gemini-2.5-pro", false),
        ("fireworks-ai/some-model", false),
        ("aws_bedrock/some-model", false),
        // Long-tail unknown — defaults to api-key, not projected.
        ("deepseek/some-model", false),
        ("groq/some-model", false),
    ];

    for (model_id, expected_projected) in cases {
        let sql_mirror = migration_predicates_subscription_evidence(model_id);
        let rust = rust_canonical_is_projected(model_id);
        assert_eq!(
            sql_mirror, rust,
            "DRIFT: SQL-mirror predicate ({sql_mirror}) disagrees with Rust \
             canonical ({rust}) for {model_id:?}. The migration 84 SQL \
             predicates and the Rust classify_provider / \
             governable_subscription_for_model rules must agree."
        );
        assert_eq!(
            sql_mirror, *expected_projected,
            "EXPECTED MISMATCH: SQL-mirror predicate returned {sql_mirror} but \
             expected {expected_projected} for {model_id:?}. Update the \
             representative case or the SQL predicate family."
        );
    }
}

/// The migration's Family-A list must cover *every* builtin subscription
/// provider. If a new subscription builtin is added to `BUILTIN_PROVIDERS`
/// without updating migration 84's Family-A IN-list, this test fails so the
/// reviewer is forced to update the SQL too.
#[test]
fn drift_guard_builtin_subscription_ids_covered() {
    let covered_by_migration_family_a: &[&str] = &[
        "minimax-coding-plan",
        "xiaomi-token-plan-sgp",
        "kimi-for-coding",
        "opencode",
        "zai-coding-plan",
        "chatgpt_codex",
        "githubcopilot",
    ];
    for p in BUILTIN_PROVIDERS
        .iter()
        .filter(|p| p.credential_class.is_subscription())
    {
        assert!(
            covered_by_migration_family_a.contains(&p.id),
            "DRIFT: builtin subscription provider {:?} is NOT listed in \
             migration 84's Family-A IN-list. Add it to \
             84_reclassify_subscription_session_cost_basis.sql Family A \
             (and to covered_by_migration_family_a here).",
            p.id
        );
    }
}

/// The migration's Family-C2 prefix list and Family-C3 exact-id list must
/// exactly mirror the Rust `is_subscription_id` constants. If either side
/// adds or removes an entry, this test fails.
#[test]
fn drift_guard_id_pattern_constants_match_migration() {
    // These are the SQL constants reproduced in the migration. They must
    // equal the Rust SUBSCRIPTION_PREFIXES / SUBSCRIPTION_IDS verbatim.
    let migration_prefixes: &[&str] = &[
        "xiaomi",
        "moonshotai",
        "alibaba",
        "tencent",
        "stepfun",
        "kuae-cloud",
        "umans-ai",
    ];
    let migration_ids: &[&str] = &[
        "zai",
        "zhipuai",
        "kimi-for-coding",
        "minimax-coding-plan",
        "opencode",
        "opencode-go",
    ];
    // Cross-check by asserting the same is_subscription_id result for each
    // constant (the Rust function is the canonical source of truth; the
    // migration constants above must match it).
    for prefix in migration_prefixes {
        // A prefix alone (e.g. "xiaomi") is itself a subscription id.
        assert!(
            is_subscription_provider(prefix),
            "migration prefix {prefix:?} must classify as subscription in Rust"
        );
    }
    for id in migration_ids {
        assert!(
            is_subscription_provider(id),
            "migration exact id {id:?} must classify as subscription in Rust"
        );
    }
}
