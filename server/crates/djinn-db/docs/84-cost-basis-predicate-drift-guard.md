# Migration 84 — Rust/SQL predicate drift guard

Migration `84_reclassify_subscription_session_cost_basis.sql` rewrites
`sessions.cost_basis` from `'actual'` to `'projected'` for historical rows
whose stored `model_id` carries durable subscription evidence. The SQL
predicates are a hand-transcription of the **Rust** subscription rules in
`djinn-provider/src/catalog/builtin.rs`:

| SQL predicate family | Rust function |
|---|---|
| Family A — builtin subscription `IN`-list | `BUILTIN_PROVIDERS` entries with `CredentialClass::Subscription` (read via `classify_provider`) |
| Family B — `openai` + `codex` marker | `governable_subscription_for_model("openai", model_id)` |
| Family C1 — `-coding-plan` / `-token-plan` / `-for-coding` segments | `is_subscription_id` suffix arm |
| Family C2 — vendor prefixes (`xiaomi`, `moonshotai`, …) | `is_subscription_id` prefix arm (`SUBSCRIPTION_PREFIXES`) |
| Family C3 — exact ids (`zai`, `zhipuai`, …) | `is_subscription_id` exact arm (`SUBSCRIPTION_IDS`) |

SQL cannot import the Rust functions, so the two copies can silently drift as
the builtin list evolves. This document is the migration-adjacent reference that
pairs each representative provider/model id with its expected SQL predicate
family **and** the canonical Rust classification, so a reviewer can confirm
alignment at a glance.

## Automated drift guard

The drift is also enforced by integration tests in
`djinn-provider/tests/cost_basis_drift_guard.rs`:

- `drift_guard_sql_predicates_match_rust_classification` — reproduces each SQL
  predicate family as a Rust helper (`migration_predicates_subscription_evidence`)
  and asserts it agrees with the canonical decision
  (`classify_provider` + `governable_subscription_for_model`) for every
  representative id below.
- `drift_guard_builtin_subscription_ids_covered` — fails if a builtin
  `Subscription` provider is added to `BUILTIN_PROVIDERS` without also being
  added to migration 84's Family-A `IN`-list.
- `drift_guard_id_pattern_constants_match_migration` — fails if the SQL C2/C3
  constants drift from the Rust `is_subscription_id` constants.

If a new builtin subscription provider is added, `drift_guard_builtin_subscription_ids_covered`
will fail at test time and the reviewer must add the id to both the migration's
Family-A `IN`-list **and** the test's `covered_by_migration_family_a` array.

## Representative provider/model matrix

`model_id` is stored in canonical `<providerID>/<modelID>` form (see
`CatalogService::find_model` / `pick_any_default_model`). The migration derives
the provider namespace via `split_part(model_id, '/', 1)`.

| model_id | expected cost_basis after migration | SQL family | Rust rule |
|---|---|---|---|
| `minimax-coding-plan/MiniMax-M3` | projected | A | builtin Subscription |
| `xiaomi-token-plan-sgp/mimo-v2.5-pro` | projected | A | builtin Subscription |
| `kimi-for-coding/k2p7` | projected | A | builtin Subscription |
| `opencode/claude-opus-4-8` | projected | A | builtin Subscription |
| `zai-coding-plan/glm-5.2` | projected | A | builtin Subscription |
| `chatgpt_codex/gpt-5.3-codex` | projected | A | builtin Subscription |
| `githubcopilot/gpt-5.3-codex` | projected | A | builtin Subscription |
| `openai/gpt-5.3-codex` | projected | B | `governable_subscription_for_model` Codex marker |
| `openai/codex-mini` | projected | B | `governable_subscription_for_model` Codex marker |
| `zhipuai-coding-plan/glm-5.2` | projected | C1 | `-coding-plan` suffix |
| `alibaba-qwen-coding-plan/qwen-max` | projected | C1 | `-coding-plan` suffix |
| `xiaomi-token-plan-cn/mimo-v2.5-pro` | projected | C1 | `-token-plan` suffix |
| `acme-for-coding/some-model` | projected | C1 | `-for-coding` suffix |
| `stepfun-ai/some-model` | projected | C2 | `stepfun` prefix |
| `kuae-cloud-coding-plan/some-model` | projected | C2 | `kuae-cloud` prefix |
| `umans-ai-coding-plan/some-model` | projected | C2 | `umans-ai` prefix |
| `moonshotai-coding-plan/some-model` | projected | C2 | `moonshotai` prefix |
| `zai/some-model` | projected | C3 | exact `zai` |
| `zhipuai/some-model` | projected | C3 | exact `zhipuai` |
| `opencode-go/some-model` | projected | C3 | exact `opencode-go` |
| `openai/gpt-5.5` | **actual** (unchanged) | — | plain OpenAI API-key, no codex marker |
| `openai/gpt-4o` | **actual** (unchanged) | — | plain OpenAI API-key, no codex marker |
| `openai/o3` | **actual** (unchanged) | — | plain OpenAI API-key, no codex marker |
| `anthropic/claude-opus-4-8` | **actual** (unchanged) | — | builtin ApiKey |
| `google/gemini-2.5-pro` | **actual** (unchanged) | — | builtin ApiKey |
| `fireworks-ai/some-model` | **actual** (unchanged) | — | builtin ApiKey |
| `aws_bedrock/some-model` | **actual** (unchanged) | — | builtin ApiKey |
| `deepseek/some-model` | **actual** (unchanged) | — | long-tail default ApiKey |
| `groq/some-model` | **actual** (unchanged) | — | long-tail default ApiKey |

### Critical non-match

Plain `openai/<non-codex>` rows must **remain `actual`**. A bare OpenAI BYO
API key is a fungible metered key, never a subscription. The migration's
Family-B predicate is scoped to `lower(model_id) LIKE '%codex%'`, so plain
OpenAI rows are excluded. This is verified by the
`governable_subscription_plain_openai_not_codex` test (sibling task f9wi) and
the `drift_guard_sql_predicates_match_rust_classification` test above.

## How to keep the guard in sync

When changing subscription rules:

1. Update the Rust rules in `djinn-provider/src/catalog/builtin.rs`
   (`BUILTIN_PROVIDERS`, `is_subscription_id`, `governable_subscription_for_model`).
2. Update the SQL predicates in
   `84_reclassify_subscription_session_cost_basis.sql` (Families A/B/C).
3. Update `covered_by_migration_family_a` / the representative matrix in
   `djinn-provider/tests/cost_basis_drift_guard.rs` if a builtin id was
   added/removed.
4. Run `cargo test -p djinn-provider --test cost_basis_drift_guard` — the drift
   guard tests must pass before the change merges.

No acceptance criterion for this work depends on live credentials, production
DB access, or operator-only proof. External rollout validation belongs in the
migration's commented validation queries (blocks 1–8 in migration 84), not in
the task acceptance criteria.
