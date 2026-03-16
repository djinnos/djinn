---
title: "ADR-031: Djinn Cloud — Middleware Integration (OpenRouter + Fireworks)"
type: adr
tags: ["adr", "billing", "inference", "monetization", "providers", "djinn-cloud", "openrouter", "fireworks"]
---

# ADR-031: Djinn Cloud — Middleware Integration

## Status: Draft

Date: 2026-03-15 (revised)

## Related

- [[ADR-032]]: Self-Hosted Open Models via Managed GPU (Level 2 — future)
- [[Djinn Cloud Strategy Overview]]

## Context

### 1. The onboarding wall kills adoption

Today, Djinn requires every user to bring their own API key from Anthropic, OpenAI, or another provider. Steps 2–4 of onboarding take 10–30 minutes and require billing relationships with third-party providers. Every successful dev tool (Cursor, Replit, Linear, Vercel) provides value within 5 minutes. Djinn currently cannot.

### 2. No revenue from the intelligence layer

Djinn's core value is agent orchestration — task planning, grooming, execution, review. But the most expensive part (LLM inference) is paid directly to providers by the user. Djinn captures zero revenue from the compute that powers its agents.

### 3. Middleware providers solve the infrastructure problem

We evaluated building our own billing proxy (~800 lines of Rust) and rejected it for the validation phase. We also evaluated LiteLLM and rejected it entirely. The right answer for a quick win is using existing middleware:

**OpenRouter** provides:
- 300+ models (frontier + open) through one OpenAI-compatible API
- Provisioning API for per-customer key creation with credit limits
- Per-key usage tracking via management API
- Automatic failover between providers
- 5.5% platform fee, no additional markup

**Fireworks AI** provides:
- Fastest open model inference (custom CUDA kernels, FireAttention)
- 4x faster structured output — critical for tool calling
- Direct pricing, no middleman fee
- Open models only (no Claude/GPT-4o)

### 4. Why NOT LiteLLM

| Issue | Evidence | Impact |
|---|---|---|
| Billing accuracy bugs | 5+ open GitHub issues (#10598, #11929, #12892, #14266, #12905) | Cannot build revenue on inaccurate metering |
| Performance ceiling | Fails at 1,000 QPS (TensorZero benchmark) | Limits growth |
| Memory leaks | Confirmed multi-version (#15128, #6404), unfixed | Operational burden |
| Security | 7 CVEs in 2024-2025: RCE (CVE-2024-6825), SQL injection (CVE-2024-5225, CVE-2025-45809), key leakage (CVE-2024-9606) | Unacceptable for billing proxy |
| Data loss risk | `prisma db push --accept-data-loss` on every startup. Full DB wipe documented (#3035) | Cannot risk billing data |
| Vendor risk | $2.1M total funding. Progressive feature-gating creep. | Dependency on underfunded company |

**OpenRouter gives us per-customer keys, usage tracking, and model routing without running any infrastructure.** LiteLLM would require Docker + Postgres + Redis + fighting its bugs.

### 5. Why NOT build a custom Rust billing proxy (the earlier plan)

The earlier version of this ADR proposed building ~800 lines of Rust for metering, budgets, and virtual keys. This was rejected because:

- **Goal is validation, not infrastructure.** 10 days to test the idea beats 3-4 weeks to build billing.
- **OpenRouter's Provisioning API already provides** per-customer keys, credit limits, and usage tracking.
- **We only need ~200-300 lines** for the credit system (sell Djinn credits, deduct from balance).
- **The custom proxy can be built later** if OpenRouter's features prove insufficient at scale. Nothing in this design prevents adding it.

### 6. Swappability

The middleware layer is fully swappable:

```
Launch:     All models → OpenRouter
Month 3+:   Open models → Fireworks direct, Frontier → OpenRouter (or direct API)
Month 6+:   Open models → self-hosted vLLM on Modal, Frontier → direct API
```

Each transition is a URL change in the model router. Customer-facing model names (`djinn-fast`, `djinn-pro`) never change.

## Decision

### Part 1: OpenRouter as primary middleware

Register `djinn_cloud` as a built-in provider in Djinn, pointing at OpenRouter's API.

**Provider registration** (`src/provider/builtin.rs`):

```rust
BuiltinProvider {
    id: "djinn_cloud",
    name: "Djinn Cloud",
    base_url: "https://openrouter.ai/api/v1",
    env_var: "DJINN_CLOUD_API_KEY",
    docs_url: "https://docs.djinn.dev/cloud",
    oauth_supported: false,
    connection_methods: vec!["signup"],
    is_openai_compatible: true,
}
```

**Model alias mapping** (`src/provider/djinn_cloud.rs` — new file, ~30 lines):

```rust
pub fn resolve_model(djinn_model: &str) -> &str {
    match djinn_model {
        "djinn-fast"  => "deepseek/deepseek-v3.2",
        "djinn-pro"   => "minimax/minimax-m2.5",
        "claude-opus" => "anthropic/claude-opus-4-6",
        "gpt-4o"      => "openai/gpt-4o",
        other         => other,  // pass-through for any model
    }
}
```

This function is the ONE place where model branding happens. Swapping `djinn-fast` to a different model is a one-line change here.

### Part 2: Per-customer key management via OpenRouter Provisioning API

On Djinn Cloud signup, our server:

1. Creates a customer record in our DB
2. Calls OpenRouter's Provisioning API: `POST /api/v1/keys` with credit limit
3. Stores the returned key ID (not the key itself — OpenRouter manages it)
4. Configures `djinn_cloud` provider with that key for the customer's Djinn instance

```rust
// Simplified — actual implementation uses reqwest
async fn provision_customer(email: &str, tier: Tier) -> Result<CustomerKey> {
    let response = openrouter_client
        .post("https://openrouter.ai/api/v1/keys")
        .json(&json!({
            "name": format!("djinn-{}", customer_id),
            "limit": tier.daily_credit_limit(),
            "limit_period": "day",
        }))
        .send()
        .await?;

    let key = response.json::<ProvisionedKey>().await?;
    db.store_customer_key(customer_id, &key.id).await?;
    Ok(key)
}
```

### Part 3: Credit system (~200 lines)

**This is the only billing code we write.** OpenRouter tracks raw token usage per key. We add a credit layer on top:

```sql
CREATE TABLE customers (
    id              TEXT PRIMARY KEY,
    email           TEXT NOT NULL UNIQUE,
    tier            TEXT NOT NULL DEFAULT 'hacker',
    stripe_id       TEXT,
    openrouter_key  TEXT NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE credit_ledger (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    customer_id     TEXT NOT NULL,
    amount          REAL NOT NULL,        -- positive = credit added, negative = deducted
    reason          TEXT NOT NULL,         -- "signup_bonus", "pro_monthly", "usage_deduction"
    balance_after   REAL NOT NULL,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    FOREIGN KEY (customer_id) REFERENCES customers(id)
);

CREATE INDEX idx_ledger_customer ON credit_ledger(customer_id, created_at);
```

**Credit flow:**
1. Free signup → add $1.50 credit (enough for ~50K tokens on djinn-fast)
2. Pro subscription → add $30 credit monthly (slightly above $29 price to cover free overshoot)
3. Periodic job (every 60 seconds): query OpenRouter per-key usage → deduct from balance
4. When balance hits $0 → return "out of credits" + upgrade prompt
5. Upgrade → Stripe Checkout → webhook → add credits → resume

**Pricing translation:**
OpenRouter charges us at provider rates + 5.5%. We sell credits at our markup:

```rust
pub fn djinn_credit_cost(openrouter_cost: f64) -> f64 {
    openrouter_cost * 3.0  // 3x markup → ~67% margin on djinn-fast
}
```

Customer buys $29 of Djinn credits. That translates to ~$9.67 of OpenRouter credits. The difference is our margin.

### Part 4: Zero-config onboarding

Desktop first-run flow (`ProviderSetupStep.tsx`) changes:

**Current:**
```
"Choose a provider" → [Anthropic] [OpenAI] [Google] ...
```

**New:**
```
┌─────────────────────────────────────────────┐
│                                             │
│  ⚡ Start with Djinn Cloud (Free)           │  ← Primary CTA, big button
│     No API key needed. Start in 30 seconds. │
│                                             │
├─────────────────────────────────────────────┤
│                                             │
│  Or use your own API key:                   │  ← Secondary, smaller
│  [Anthropic] [OpenAI] [Google] ...          │
│                                             │
└─────────────────────────────────────────────┘
```

Sign-up: email → create customer → provision OpenRouter key → auto-configure `djinn_cloud` provider with sensible model priorities → done.

### Part 5: Usage dashboard

New desktop component showing:
- Credits remaining / daily allocation
- Breakdown by model (djinn-fast, djinn-pro, frontier)
- "Upgrade to Pro" CTA when credits run low

Data sourced from: our `credit_ledger` table + OpenRouter's per-key usage API.

### Part 6: Stripe integration

- **Pro subscription:** Stripe Checkout → `customer.subscription.created` webhook → update tier + add credits
- **Monthly credit refresh:** Cron or Stripe webhook on billing cycle → add $30 credits
- **Overage (Team tier):** Usage beyond credit allocation billed at published per-token rate via Stripe Meter API

### Part 7: Fireworks direct integration (Month 3+)

When open model spend through OpenRouter exceeds $3-5K/mo, add Fireworks as a direct backend for `djinn-fast` and `djinn-pro`:

```rust
pub fn resolve_model(djinn_model: &str) -> UpstreamTarget {
    match djinn_model {
        // Open models → Fireworks (no middleman fee, faster)
        "djinn-fast" => UpstreamTarget {
            base_url: "https://api.fireworks.ai/inference/v1",
            model_id: "accounts/fireworks/models/deepseek-v3p2",
            auth: AuthMethod::BearerToken(our_fireworks_key),
        },
        "djinn-pro" => UpstreamTarget {
            base_url: "https://api.fireworks.ai/inference/v1",
            model_id: "accounts/fireworks/models/minimax-m2p5",
            auth: AuthMethod::BearerToken(our_fireworks_key),
        },
        // Frontier → OpenRouter (or direct API)
        "claude-opus" => UpstreamTarget {
            base_url: "https://openrouter.ai/api/v1",
            model_id: "anthropic/claude-opus-4-6",
            auth: AuthMethod::BearerToken(our_openrouter_key),
        },
        // ...
    }
}
```

**When Fireworks is used directly, we lose OpenRouter's per-key tracking.** At that point, we build our own token counting (~100 additional lines of Rust): extract `usage.prompt_tokens` / `usage.completion_tokens` from the provider response and log to our `credit_ledger`.

This is the "build billing later when justified" approach — only add the custom metering code when we actually need it.

## Server files that change

### New files

| File | Lines (est.) | Purpose |
|---|---|---|
| `src/provider/djinn_cloud.rs` | ~60 | Model alias mapping, customer provisioning |
| `src/billing/mod.rs` | ~40 | Credit system public API |
| `src/billing/credits.rs` | ~120 | Credit ledger, balance check, deduction |
| `src/billing/stripe.rs` | ~80 | Webhook handler, subscription management |
| `src/mcp/tools/billing_tools.rs` | ~60 | MCP tools: `billing_usage`, `billing_upgrade` |
| `migrations/V{next}__billing.sql` | ~30 | customers + credit_ledger tables |

### Modified files

| File | Change | Lines |
|---|---|---|
| `src/provider/builtin.rs` | Add `djinn_cloud` entry | ~15 |
| `src/actors/slot/helpers.rs` | Call `resolve_model()` for djinn_cloud provider | ~10 |
| `src/actors/slot/lifecycle.rs` | Credit check before dispatch (djinn_cloud only) | ~15 |
| `src/lib.rs` | Add `pub mod billing;` | 1 |
| `src/mcp/dispatch.rs` | Register billing MCP tools | ~5 |
| `src/events.rs` | Add billing events for SSE | ~15 |

### Desktop files

| File | Change | Lines |
|---|---|---|
| `src/components/ProviderSetupStep.tsx` | Add "Start with Djinn Cloud" CTA + signup form | ~100 |
| `src/components/UsageDashboard.tsx` | New — credit balance, model breakdown | ~150 |
| `src/pages/SettingsPage.tsx` | Add Billing tab | ~30 |
| `src/hooks/settings/useBilling.ts` | New — billing data hook | ~60 |
| `src/stores/billingStore.ts` | New — Zustand store for credits/usage | ~50 |

### Total

| Category | Files | Lines |
|---|---|---|
| New Rust | 5 | ~360 |
| Modified Rust | 5 | ~60 |
| New migration | 1 | ~30 |
| New TypeScript | 3 | ~260 |
| Modified TypeScript | 2 | ~130 |
| **Total** | **16 files** | **~840 lines** |

Half the code of the previous plan, and most of it is desktop UI.

## Consequences

### Positive

1. **Time-to-magic drops from 30 minutes to 5 minutes.**
2. **Revenue from day one.** 67% margin on djinn-fast, 60% on djinn-pro, 30% on frontier.
3. **Zero infrastructure to manage.** OpenRouter handles model hosting, routing, failover.
4. **BYOK remains supported.** Existing users unaffected. Djinn Cloud is additive.
5. **Model swaps are one-line changes.** New model appears? Update `resolve_model()`. Done.
6. **Upgrade path is clear.** Fireworks direct at Month 3. Self-hosted at Month 6+. No rewrite needed.

### Negative

1. **Dependency on OpenRouter.** If they go down, Djinn Cloud goes down. Mitigated by Fireworks as backup.
2. **5.5% middleman fee.** Real cost. Justified by zero infra ops. Eliminated when we move to Fireworks direct.
3. **Anthropic ToS gray area.** Reselling Claude via OpenRouter isn't explicitly approved. Low risk at small scale. Mitigated by BYOK fallback and AWS Bedrock option at scale.

### Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| OpenRouter outage | Medium | All Djinn Cloud users affected | Fireworks as fallback for open models |
| djinn-fast quality insufficient | Medium | Users don't trust "Auto mode" | Test with 50+ MCP tools before launch. Swap model if needed. |
| Free tier abuse | High | Token cost hemorrhage | Email verification + IP rate limit + progressive trust |
| Anthropic ToS enforcement | Low | Can't resell Claude | BYOK fallback. Bedrock reseller channel. Direct agreement. |

## Testing Plan

### Phase 1: Model validation (Day 1-3)

Before shipping anything, test djinn-fast and djinn-pro against Djinn's actual MCP tools:

1. Run 50 representative agent tasks through DeepSeek V3.2 (djinn-fast candidate)
2. Run 50 through MiniMax M2.5 (djinn-pro candidate)
3. Measure: tool call accuracy, JSON schema compliance, task completion rate

**Hard gates:**
- Tool call accuracy >90%
- JSON schema compliance >95%
- If not met: pick a different model. Qwen3 Coder and Kimi K2 are ready alternatives.

### Phase 2: Internal dogfood (Day 8-10)

Team uses Djinn Cloud for 3 days. Verify:
- Credit deduction matches OpenRouter reported usage (within 5%)
- Onboarding flow works end-to-end
- Usage dashboard shows correct data
- Upgrade flow works via Stripe

### Phase 3: Alpha (Week 3-4)

10-20 selected users. Mix of new users and existing BYOK users. Monitor:
- Signup → first agent task time (<10 min target)
- Credit accuracy vs OpenRouter usage API
- Free tier daily cap behavior
- Any quality complaints about djinn-fast/djinn-pro

### Phase 4: Public launch (Week 5-6)

- Remove alpha gate
- Enable Stripe billing
- Announce

## Implementation Order

```
DAY  1    Add djinn_cloud to BUILTIN_PROVIDERS
          Create resolve_model() alias mapping
          OpenRouter Provisioning API integration (create key, set limits)

DAY  2    Credit system: customers table, credit_ledger, balance check
          Credit deduction job (query OpenRouter usage → deduct)

DAY  3    Test djinn-fast (DeepSeek V3.2) against Djinn MCP tools
          Test djinn-pro (MiniMax M2.5) against Djinn MCP tools
          Swap models if quality gates not met

DAY  4    Stripe Checkout for Pro tier
          Stripe webhook → tier change → credit refresh
          Billing MCP tools (billing_usage, billing_upgrade)

DAY  5-6  Desktop: ProviderSetupStep "Start with Djinn Cloud" CTA
          Desktop: sign-up form (email → provision → auto-config)
          Desktop: UsageDashboard component

DAY  7    Desktop: upgrade prompt when credits low
          Desktop: Billing tab in SettingsPage
          Wire SSE events for real-time usage updates

DAY  8-10 Internal dogfood
          Fix issues
          Validate credit accuracy against OpenRouter

WEEK 3-4  Alpha with 10-20 users
WEEK 5-6  Public launch
```

## Part 8: Custom Rust Billing Proxy — When and Why to Build It

The custom Rust billing proxy (metering, budget enforcement, virtual keys) was the original plan for this ADR. It was deferred in favor of OpenRouter's middleware for the validation phase. But it remains a viable and eventually necessary option. This section documents exactly when to build it and what triggers each piece.

### The billing code grows incrementally, not all at once

```
MONTH 1-3:   OpenRouter handles everything.
             Build: nothing. Credit system only (~200 lines).

MONTH 3-6:   Fireworks direct for open models.
             Build: token counter (~100 lines).
             Why: Fireworks has no per-customer tracking. We need to extract
             usage.prompt_tokens from each response and attribute to the customer.

MONTH 6-12:  Serious revenue, enterprise customers.
             Build: full metering + budget enforcement (~300 more lines).
             Why: Enterprise customers at $10K/mo want per-request audit logs,
             not "OpenRouter said so." We need our own source of truth.

YEAR 2:      Multi-provider routing, high volume.
             Build: the full proxy with virtual keys (~200 more lines).
             Why: At $50K+/mo, the 5.5% OpenRouter fee is $2,750/mo — an
             engineer's salary going to a middleman for HTTP routing.
             We route directly to all providers and own the entire billing stack.
```

### Trigger 1: Fireworks direct → Token counter (~100 lines)

**When:** Open model spend through OpenRouter exceeds $3-5K/mo.

**What you build:** After each response from Fireworks, extract token counts and log them:

```rust
// In lifecycle.rs, after streaming response completes
if is_direct_provider(provider_id) {  // Fireworks, Anthropic direct, etc.
    let usage = extract_token_usage(&response);
    billing::record_usage(UsageEvent {
        customer_id,
        model_id,
        tokens_in: usage.prompt_tokens,
        tokens_out: usage.completion_tokens,
        cost: pricing.calculate(model_id, &usage),
    }).await;
}
```

**New files:**
- `src/billing/metering.rs` (~100 lines) — token extraction + logging

**Why not before:** OpenRouter does this for us via per-key usage API. Only needed when we bypass OpenRouter.

### Trigger 2: Enterprise customers → Full metering + budgets (~300 lines)

**When:** First enterprise customer at $5K+/mo, or when Djinn Cloud revenue exceeds $20K MRR.

**What you build:**
- Per-request audit logs (every request logged with tokens, cost, model, timestamp)
- Budget enforcement (hard limits per customer, not just credit balance)
- Usage aggregation queries (cost-per-task, cost-per-role, cost-per-model breakdowns)
- Reconciliation checks (compare our logs against provider invoices)

**New files:**
- `src/billing/budget.rs` (~150 lines) — budget check before dispatch, 429 on limit
- `src/billing/audit.rs` (~150 lines) — per-request audit log, reconciliation queries

**Why not before:** Credits + OpenRouter usage API are sufficient for Free/Pro tiers. Enterprise customers need guarantees and audit trails.

### Trigger 3: Eliminate middleman → Full proxy with routing (~200 lines)

**When:** Total inference spend exceeds $50K/mo, OR OpenRouter becomes unreliable/raises fees.

**What you build:**
- Virtual key system (`sk-djinn-xxx` per customer) replacing OpenRouter Provisioning API
- Multi-provider router (call Anthropic, OpenAI, Fireworks directly — no middleware)
- Rate limiting per customer (token bucket, in-memory)
- Provider failover (Claude down? → route to GPT-4o)

**New/expanded files:**
- `src/billing/keys.rs` (~120 lines) — virtual key generation, hashing, validation
- `src/billing/router.rs` (~80 lines) — multi-provider routing with failover
- Expand `src/billing/budget.rs` with rate limiting

**Why not before:** At $50K/mo, you're paying OpenRouter $2,750/mo to route HTTP requests. The Rust code pays for itself in month one. Before that, the convenience of OpenRouter's per-customer keys and usage tracking is worth the 5.5%.

### Total code at each stage

| Stage | Cumulative Rust (billing) | What it does |
|---|---|---|
| Launch (Month 1) | ~200 lines | Credit system only. OpenRouter does the rest. |
| Fireworks direct (Month 3) | ~300 lines | + Token counter for direct providers |
| Enterprise (Month 6-12) | ~600 lines | + Budget enforcement + audit logs |
| Full proxy (Year 2) | ~800 lines | + Virtual keys + multi-provider routing + rate limiting |

**This is the same ~800 lines from the original plan.** The difference is we build it over 12-18 months as each trigger hits, not all at once before we have a single customer.

### Why Rust specifically (not Python/Node)

The billing proxy runs inside the existing Djinn server process. The server is Rust. There is no second service to deploy, no Docker container, no new database. The billing module hooks into the existing `lifecycle.rs` dispatch and the existing SQLite database. Adding it in any other language would mean deploying and operating a separate service — which is exactly the operational complexity we're avoiding.

### The key architectural property: each stage is additive

```
Stage 1: OpenRouter API → credit_ledger
Stage 2: OpenRouter API → credit_ledger + metering.rs (for Fireworks)
Stage 3: OpenRouter API → credit_ledger + metering.rs + budget.rs + audit.rs
Stage 4: direct APIs   → credit_ledger + metering.rs + budget.rs + audit.rs + keys.rs + router.rs
```

Nothing gets rewritten. Each stage adds a module. The credit system from Stage 1 is still there in Stage 4. The token counter from Stage 2 is still there in Stage 4. You never throw away code.

---

## References

- OpenRouter Provisioning API: per-customer key creation with credit limits
- OpenRouter pricing: provider rates + 5.5% platform fee
- Fireworks AI: $254M raised, $4B valuation. Custom CUDA kernels. Cursor/Notion use them.
- LiteLLM rejection evidence: GitHub issues #10598, #11929, #12892, #14266, #12905, #15128, #3035. CVEs: CVE-2024-6825, CVE-2024-5225, CVE-2025-45809.
- DeepSeek V3.2: 67-74% SWE-bench, $0.26/$0.38 per MTok on OpenRouter
- MiniMax M2.5: 80.2% SWE-bench, $0.25/$1.20 per MTok on OpenRouter

## Relations

- [[ADR-032]]: Self-Hosted Open Models (Level 2)
- [[Djinn Cloud Strategy Overview]]
