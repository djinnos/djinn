---
title: "Djinn Cloud Strategy Overview — Inference Middleware & Model Offering"
type: design
tags: ["strategy", "inference", "monetization", "djinn-cloud", "pricing", "go-to-market", "openrouter", "fireworks"]
---

# Djinn Cloud Strategy Overview

Date: 2026-03-15 (revised)

## Related ADRs

- [[ADR-031]]: Djinn Cloud — Middleware Integration (OpenRouter + Fireworks)
- [[ADR-032]]: Scaling — Self-Hosted Models via Managed GPU (future)

---

## Executive Summary

Djinn Cloud transforms Djinn from a BYOK (Bring Your Own Key) desktop tool into a monetized AI platform. Instead of requiring users to manage their own LLM provider accounts, Djinn Cloud provides zero-config inference with built-in billing.

**The core insight:** We don't build inference infrastructure. We use middleware providers (OpenRouter, Fireworks AI, or similar) that already host and serve models, and we add our branding, billing, and routing on top.

**Three levels of infrastructure investment, each building on the last:**

```
┌──────────────────────────────────────────────────────────────────────┐
│                                                                      │
│  LEVEL 1: Use someone else's everything          Ship: Week 1-2     │
│  ────────────────────────────────────                                │
│  OpenRouter or Fireworks serves the models.                          │
│  We add: branding, credits, billing.                                 │
│  Our infra: just the Djinn server (already exists).                  │
│                                                                      │
│  Effort: ~10 days        Margin: 30-60%        Risk: Low            │
│                                                                      │
├──────────────────────────────────────────────────────────────────────┤
│                                                                      │
│  LEVEL 2: Rent GPUs, run our own model server    Ship: Month 3+     │
│  ────────────────────────────────────────────                        │
│  vLLM on Modal Labs / RunPod. Our fine-tuned models.                 │
│  OpenRouter stays for frontier models.                               │
│  Our infra: Djinn server + one Modal deployment.                     │
│                                                                      │
│  Effort: 4-6 weeks      Margin: 60-80%         Risk: Medium         │
│                                                                      │
├──────────────────────────────────────────────────────────────────────┤
│                                                                      │
│  LEVEL 3: Own the full stack (Fireworks-level)   NEVER               │
│  ──────────────────────────────────────────────                      │
│  Custom CUDA kernels, GPU fleet, multi-tenant serving.               │
│  This is what Fireworks ($254M raised) and Together built.           │
│  We don't need this. We never build this.                            │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘
```

**Level 1 is the quick win.** Validate the idea in 10 days. See if customers use Djinn Cloud, if djinn-fast is good enough, if free users convert to Pro.

**Level 2 is the margin play.** Only pursue after Level 1 proves demand. Self-hosted models on rented GPUs push margins from 30% to 80%.

**Level 3 is someone else's business.** We never build Fireworks. We use Fireworks (or its competitors).

---

## How Models "Become Ours"

This is the single most important concept in this strategy.

**We don't train models. We don't host models. We brand models.**

The customer sees:

```
Djinn Cloud Models:
├── djinn-fast     "Fast, efficient for routine tasks"
├── djinn-pro      "Advanced coding and planning"
├── claude-opus    "Anthropic Claude Opus" (premium)
└── gpt-4o         "OpenAI GPT-4o" (premium)
```

What's actually happening:

```
Customer-facing name     What it actually is              Who serves it
────────────────────     ──────────────────────           ─────────────
djinn-fast           →   DeepSeek V3.2                →   OpenRouter (or Fireworks)
djinn-pro            →   MiniMax M2.5                 →   OpenRouter (or Fireworks)
claude-opus          →   Claude Opus 4.6              →   OpenRouter → Anthropic
gpt-4o               →   GPT-4o                       →   OpenRouter → OpenAI
```

The left column is what we own. Everything to the right is an implementation detail we can change without the customer knowing. If a better model comes out tomorrow, we swap `djinn-fast` to point at it. Customer sees the same name, same API key, same billing.

**This is exactly what Cursor does.** You don't pick "Anthropic" in Cursor. You pick "claude-3.5-sonnet" from Cursor's model list. Cursor is the brand. The model provider is invisible.

---

## Why These Two Middleware Providers

### OpenRouter — The all-in-one option

| | |
|---|---|
| **What** | Single API routing to 300+ models (frontier + open) |
| **Key feature** | Provisioning API — create per-customer keys with credit limits |
| **Cost** | Provider rates + 5.5% platform fee |
| **Models** | Claude, GPT-4o, DeepSeek, Qwen, Llama, MiniMax, everything |
| **Per-customer tracking** | Yes — per-key usage via management API |
| **Why it fits** | One integration covers everything. Fastest path to launch. |

### Fireworks AI — The performance option

| | |
|---|---|
| **What** | Fastest open-model inference (custom CUDA kernels) |
| **Key feature** | FireAttention — 4x faster structured output. Best for tool calling. |
| **Cost** | Direct pricing, no middleman fee |
| **Models** | Open models only (DeepSeek, Llama, Qwen, etc.). No Claude/GPT-4o. |
| **Per-customer tracking** | No — account-level only. We build our own tracking. |
| **Why it fits** | Faster for our branded models. No middleman tax. |

### Why not pick just one?

Because they complement each other:

```
OPTION A: OpenRouter only (simplest)
    All models through one API. 5.5% fee on everything. Per-customer keys for free.

OPTION B: Fireworks + OpenRouter (optimal at scale)
    djinn-fast / djinn-pro → Fireworks (fastest, no middleman fee)
    claude / gpt-4o        → OpenRouter (frontier coverage)

OPTION C: Fireworks + direct Anthropic/OpenAI (maximum margin)
    djinn-fast / djinn-pro → Fireworks
    claude-opus            → Anthropic API direct (our key)
    gpt-4o                 → OpenAI API direct (our key)
```

**Start with Option A.** Move to B when open model spend exceeds $3-5K/mo (saves the 5.5% fee on the bulk of traffic). Option C only if you want to eliminate the OpenRouter dependency entirely.

---

## The Models — What We Offer

### `djinn-fast` → DeepSeek V3.2

| | |
|---|---|
| **OpenRouter ID** | `deepseek/deepseek-v3.2` |
| **Our cost** | $0.26 input / $0.38 output per MTok |
| **We charge** | $0.80 / $1.20 (3x markup) |
| **Margin** | ~67% |
| **SWE-bench** | 67-74% |
| **Context** | 163K tokens |
| **Tool calling** | Full (auto, none, required, function) |
| **Why** | Cheapest model that codes well. Great for routine agent tasks. |

### `djinn-pro` → MiniMax M2.5

| | |
|---|---|
| **OpenRouter ID** | `minimax/minimax-m2.5` |
| **Our cost** | $0.25 input / $1.20 output per MTok |
| **We charge** | $0.80 / $3.00 (2.5x markup) |
| **Margin** | ~60% |
| **SWE-bench** | **80.2%** (highest open model available) |
| **Context** | 196K tokens |
| **Tool calling** | Yes |
| **Why** | Near-Claude quality at 1/3 the price. Best for complex tasks. |

### Alternatives evaluated and available as swaps

| Model | SWE-bench | Cost (output/MTok) | Why it's a backup |
|---|---|---|---|
| Kimi K2 | 65.8% | $2.20 | Good tool calling reputation, but 2x cost of MiniMax for lower scores |
| Qwen3 Coder | 66.5% | $1.00 | Purpose-built for coding. 262K context. Strong alternative to djinn-fast |
| GLM-4.7 | N/A | $1.98 | No published coding benchmarks. Hard to justify. |
| Llama 4 Scout | N/A | $0.30 | Very cheap but no coding benchmark evidence |
| Codestral 2508 | N/A | $0.90 | Good for FIM/autocomplete. Not for agentic tool calling. |

**Models can be swapped at any time.** The alias mapping (`djinn-fast` → `deepseek/deepseek-v3.2`) is a one-line config change. If a better model appears, we switch. Customer sees the same name.

---

## Tools Evaluated — Complete Landscape

### Inference Providers (Layer 1: who serves models)

| Provider | Role in our plan | Open models | Frontier | Per-customer keys | Notes |
|---|---|---|---|---|---|
| **OpenRouter** ✓ | Primary middleware | Yes (300+) | Yes | Yes (Provisioning API) | $40M raised, $500M val. Best all-in-one. |
| **Fireworks AI** ✓ | Direct open model backend (at scale) | Yes (fastest) | No | No | $254M raised, $4B val. Cursor uses them. |
| **Together AI** | Alternative to Fireworks | Yes (200+) | No | No | VPC option for white-label. |
| **DeepInfra** | Cheapest backend | Yes (100+) | No | No | $0.03-0.20/MTok. Best for max markup. |
| **Anthropic** | Direct Claude access | No | Claude only | No | Use via OpenRouter or direct. |
| **OpenAI** | Direct GPT access | No | GPT only | No | Use via OpenRouter or direct. |
| **Groq** | Fastest inference (LPU) | Yes (limited) | No | No | No fine-tune support. Speed-first. |

### LLM Gateways (Layer 2: routing, metering, billing)

| Gateway | Why considered | Why accepted/rejected |
|---|---|---|
| **OpenRouter** ✓ | Per-customer keys, usage tracking, 300+ models | **Accepted.** Best quick-win middleware. |
| **LiteLLM** ✗ | Most commonly referenced | **Rejected.** 5+ billing bugs, 7 CVEs, memory leaks, fails at 1K QPS. See ADR-031. |
| **LLM Gateway.io** | White-label with markup controls | Interesting. Purpose-built for SaaS resale. Less proven. Monitor. |
| **Portkey** | Best observability | Good for cost attribution. Budget limits paywalled at $2K+/mo. |
| **TensorZero** | 10K QPS, Rust-based | Best performance. No billing features. Viable future routing layer. |
| **Kong AI Gateway** | 23K RPS, enterprise-grade | Overkill for current stage. Revisit at 1K+ customers. |
| **Cloudflare AI Gateway** | Free edge proxy | Not self-hostable. 100K log cap. No billing features. |
| **Helicone** | Observability-first | Not a billing/routing solution. Good companion, not replacement. |

### Billing Infrastructure

| Tool | Our choice | Notes |
|---|---|---|
| **Stripe** ✓ | Subscriptions + Checkout + Meter API | Industry standard. Handles Pro tier billing. |
| **Lago** (OSS) | Alternative if Stripe Meter API insufficient | Event-driven usage billing. 8.7K GitHub stars. |
| **Stripe Token Billing** | Monitor | Private preview. May simplify our integration when GA. |

### Future Scaling (Level 2 — only if validated)

| Tool | Purpose | When to consider |
|---|---|---|
| **Modal Labs** | Serverless GPU for self-hosted models | When open model spend >$5K/mo through OpenRouter |
| **SkyPilot** | Multi-cloud GPU orchestrator | When GPU spend >$5K/mo on Modal |
| **vLLM** | Model serving engine | The serving layer on Modal/RunPod |
| **RunPod** | Alternative GPU provider | If Modal pricing becomes unfavorable |

---

## Why Not LiteLLM

LiteLLM Proxy was the first candidate evaluated as the billing/routing layer. It was rejected. Full evidence in ADR-031, but summary:

| Issue | Evidence |
|---|---|
| Billing accuracy bugs | 5+ open issues: spend recorded as $0, budget bypass, undercounting |
| Performance ceiling | Fails at 1,000 QPS (TensorZero benchmark) |
| Memory leaks | Confirmed multi-version, unfixed. Official workaround: process recycling. |
| Security | 7 CVEs in 2024-2025 including RCE and SQL injection |
| Data loss risk | `prisma db push --accept-data-loss` runs on every startup |
| Vendor risk | $2.1M total funding. Progressive feature-gating without announcement. |

**OpenRouter gives us per-customer keys, usage tracking, and 300+ model routing without running any infrastructure.** LiteLLM would require us to run a Docker container, Postgres, and Redis — and then fight its bugs.

---

## Pricing Tiers

### Launch (Level 1)

| | Hacker (Free) | Pro ($29/mo) |
|---|---|---|
| Projects | 1 | 5 |
| Agent roles | All | All |
| Daily tokens | 50K (~25-30 tasks) | 1M (~500 tasks) |
| `djinn-fast` | Yes | Unlimited |
| `djinn-pro` | — | 500K tokens/day |
| Frontier (Claude/GPT-4o) | — | 200K tokens/day |
| BYOK option | No | No |

**Why two tiers only:** Less decision paralysis. Learn conversion triggers before adding Team/Enterprise.

### Expanded (after validation)

| | Hacker (Free) | Pro ($29/mo) | Team ($79/seat/mo) | Enterprise |
|---|---|---|---|---|
| Projects | 1 | 5 | Unlimited | Unlimited |
| `djinn-fast` | 50K/day | Unlimited | Unlimited | Unlimited |
| `djinn-pro` | — | 500K/day | 2M/seat/day | Custom |
| Frontier | — | 200K/day | 1M/seat/day | Custom |
| BYOK | No | No | Yes | Yes |

### Pricing philosophy

1. **Gate on volume, not features.** Developers hate feature gates. They accept usage limits.
2. **`djinn-fast` unlimited on paid tiers.** Costs us ~$0.60/user/month. Feels generous.
3. **Frontier models are the premium resource.** 33% margin — meter carefully.
4. **BYOK on Team tier** removes the "why pay markup?" objection.
5. **$29/mo** is the "don't think about it" price point for solo developers.

---

## Architecture

### Level 1 (Launch — OpenRouter only)

```
┌───────────────┐
│ Djinn Desktop │
└───────┬───────┘
        │ MCP over HTTP
        ▼
┌──────────────────────────────────────┐
│ Djinn Server                         │
│                                      │
│ ┌──────────┐  ┌────────────────────┐ │
│ │ Credits  │  │ Model Alias        │ │
│ │ & Billing│  │ Mapping            │ │
│ │ (~200 ln)│  │ djinn-fast → DS V3 │ │
│ └────┬─────┘  │ djinn-pro → MM M2.5│ │
│      │        └────────┬───────────┘ │
│      │                 │             │
└──────┼─────────────────┼─────────────┘
       │                 │
       │                 ▼
       │        ┌─────────────────┐
       │        │  OpenRouter API │  ← Our API key, not customer's
       │        │  (300+ models)  │
       │        └────────┬────────┘
       │                 │
       │         ┌───────┼───────┐
       │         ▼       ▼       ▼
       │     Anthropic OpenAI  Fireworks/
       │     (Claude) (GPT-4o) Together/etc
       │                       (DeepSeek, MiniMax)
       ▼
 ┌──────────┐
 │ Stripe   │ (Pro subscription)
 └──────────┘
```

### Level 1b (At scale — Fireworks direct for open models)

```
┌───────────────┐
│ Djinn Desktop │
└───────┬───────┘
        │
        ▼
┌──────────────────────────────────────────┐
│ Djinn Server                             │
│                                          │
│ ┌──────────┐  ┌─────────────────────┐   │
│ │ Credits  │  │ Model Router        │   │
│ │ & Billing│  │                     │   │
│ └────┬─────┘  │ djinn-fast ─→ FW   │   │
│      │        │ djinn-pro  ─→ FW   │   │
│      │        │ claude     ─→ OR   │   │
│      │        │ gpt-4o     ─→ OR   │   │
│      │        └──┬──────────┬──────┘   │
│      │           │          │          │
└──────┼───────────┼──────────┼──────────┘
       │           │          │
       │           ▼          ▼
       │    ┌──────────┐ ┌─────────────┐
       │    │ Fireworks│ │ OpenRouter  │
       │    │ (open)   │ │ (frontier)  │
       │    └──────────┘ └─────────────┘
       ▼
 ┌──────────┐
 │ Stripe   │
 └──────────┘
```

### Level 2 (Future — self-hosted open models)

```
┌───────────────┐
│ Djinn Desktop │
└───────┬───────┘
        │
        ▼
┌──────────────────────────────────────────────┐
│ Djinn Server                                 │
│                                              │
│ ┌──────────┐  ┌─────────────────────────┐   │
│ │ Credits  │  │ Model Router            │   │
│ │ & Billing│  │                         │   │
│ └────┬─────┘  │ djinn-fast ─→ Modal/vLLM│   │
│      │        │ djinn-pro  ─→ Modal/vLLM│   │
│      │        │ claude     ─→ OR or API │   │
│      │        │ gpt-4o     ─→ OR or API │   │
│      │        └──┬──────┬───────┬───────┘   │
│      │           │      │       │           │
└──────┼───────────┼──────┼───────┼───────────┘
       │           │      │       │
       │           ▼      ▼       ▼
       │      ┌───────┐ ┌────┐ ┌─────────┐
       │      │ Modal │ │ OR │ │Anthropic│
       │      │ vLLM  │ │    │ │ direct  │
       │      │(ours) │ │    │ │         │
       │      └───────┘ └────┘ └─────────┘
       ▼
 ┌──────────┐
 │ Stripe   │
 └──────────┘
```

---

## The Three Levels — Clearly

### Level 1: Use someone else's everything

**What it is:** We call OpenRouter's API. They route to the actual model providers. We add branding and billing.

**Our infra:** Just the Djinn server. No new containers, no databases, no GPU management.

**What we build:** ~200-300 lines of code:
- Model alias mapping (djinn-fast → deepseek/deepseek-v3.2)
- OpenRouter Provisioning API integration (create per-customer keys)
- Credit system (sell credits, deduct based on OpenRouter usage)
- Desktop UI (sign-up, usage dashboard, upgrade prompt)

**What we can swap:** OpenRouter → Fireworks, Together, DeepInfra, or any other provider. URL change. Customer sees nothing.

**What we CAN'T do:** Serve models faster, cheaper, or differently than our provider serves them. We're renting access.

**Timeline:** 10 days to ship.

### Level 2: Rent GPUs, run our own model server

**What it is:** We deploy vLLM on Modal Labs (serverless GPU). We download open model weights from HuggingFace, optionally fine-tune them on Djinn's tool schema, and serve them ourselves.

**Our infra:** Djinn server + a Modal deployment (Python, managed by Modal).

**What we build:** Modal deployment config, model eval suite, complexity router, fine-tuning pipeline.

**What we can swap:** Modal → RunPod, Lambda Labs, SkyPilot. The serving stack (vLLM) runs anywhere.

**What we own:** The fine-tuned model weights, the serving configuration, the routing logic. This is "our model" in the real sense.

**When to do this:** Only after Level 1 validates demand. Trigger: open model spend through OpenRouter exceeds $5K/month.

**Timeline:** 4-6 weeks once triggered.

### Level 3: Own the full stack

**What it is:** Custom CUDA kernels, GPU fleet, multi-tenant model serving. What Fireworks, Together, and DeepInfra built.

**We never do this.** It requires $50-300M and dozens of ML engineers. We use these companies. We don't compete with them.

---

## Go-to-Market

### Onboarding funnel

```
Download Djinn Desktop
    │
    ▼
"Start with Djinn Cloud (Free)"     ← Primary CTA
    │   "Use your own API key"       ← Secondary, smaller
    ▼
Email signup (30 seconds)
    │
    ▼
Auto-configured — djinn_cloud provider set up with customer's key
    │
    ▼
"Import from GitHub" → Djinn scans repo, creates initial board
    │
    ▼
First agent task runs → User sees the magic
    │
    ▼
TIME TO MAGIC: < 5 MINUTES
```

### Conversion triggers

| Trigger | What happens |
|---|---|
| Daily cap hit | "Upgrade to Pro" with 1-click Stripe Checkout |
| Multi-project need | "Pro unlocks 5 projects" |
| Frontier model access | "This task would benefit from Claude — available on Pro" |
| Team collaboration | "Team tier: $79/seat" |

### Developer channels

| Channel | Tactic |
|---|---|
| GitHub | Open-source the desktop client. Server + inference = proprietary. |
| Hacker News | Launch: "AI that plans, grooms, and executes your GitHub issues" |
| Twitter/X | Weekly demos: "Watch Djinn groom 10 tasks in 2 minutes" |
| Discord | Community with `#show-your-board` |
| GitHub Action | `djinn-ci` — auto-groom PRs. Viral loop. |

---

## Economics

### Unit economics per user per month

**Level 1 (OpenRouter):**

| User type | Revenue | Our cost | Margin |
|---|---|---|---|
| Free (light, 20K tokens/day) | $0 | ~$0.30/mo | Loss leader |
| Free (heavy, hits 50K cap daily) | $0 | ~$0.60/mo | Loss — but converts |
| Pro (average) | $29 | ~$12 | **$17 profit** |
| Pro (power user) | $29 | ~$22 | **$7 profit** |

**Level 2 (self-hosted open models, frontier via OpenRouter):**

| User type | Revenue | Our cost | Margin |
|---|---|---|---|
| Free | $0 | ~$0.10/mo | Negligible loss |
| Pro (average) | $29 | ~$5 | **$24 profit** |
| Pro (power user) | $29 | ~$10 | **$19 profit** |

### Break-even

| Milestone | Pro users needed | Monthly net |
|---|---|---|
| Infra break-even | 3 | +$0 |
| Cash-flow positive | 10 | +$70 |
| Meaningful revenue | 50 | +$850 |
| Level 2 justified | 100 | +$1,700 |

---

## Complete Timeline

```
DAY  1-2   Model alias mapping + OpenRouter Provisioning API
DAY  3-4   Credit system + usage tracking
DAY  5-7   Desktop: sign-up flow, usage dashboard, upgrade prompt
DAY  8-10  Dogfood internally + fix issues
─── DJINN CLOUD ALPHA ─── (10-20 selected users)
WEEK 3-4   Alpha monitoring, validate metering accuracy
WEEK 5-6   Iterate, fix, polish onboarding
─── DJINN CLOUD PUBLIC ─── (Free + Pro tiers)
MONTH 3    Evaluate: move open models to Fireworks direct? (saves 5.5%)
MONTH 3+   IF demand validated: begin Level 2 (Modal + vLLM)
MONTH 6    Team tier if demand signal exists
```

---

## Risk Register

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| 1 | OpenRouter outage | Medium | All Djinn Cloud users affected | Fireworks as backup for open models. Or direct Anthropic/OpenAI for frontier. |
| 2 | Anthropic ToS enforcement | Low | Can't resell Claude via OpenRouter | BYOK fallback. Route Claude through AWS Bedrock (official reseller channel). Or get direct Anthropic agreement. |
| 3 | Free tier abuse | High | Token cost hemorrhage | Email verification + IP rate limit + progressive trust (10K tokens first 24h). |
| 4 | djinn-fast quality insufficient | Medium | Users don't trust "Auto mode" | Test with Djinn's 50+ MCP tools before launch. If <90% tool call accuracy, pick different model. |
| 5 | OpenRouter raises fees | Low | Margin squeeze | Move to Fireworks direct (already planned for Month 3). |
| 6 | Better model released | High | Our model picks become stale | One-line alias swap. Update config, customer sees nothing. |
| 7 | Pricing wrong | Medium | Too high = no users, too low = no margin | Start at $29/mo. A/B test. Adjust monthly. |

---

## What Can Change (and how)

Every component is replaceable. This is by design.

| Component | Current choice | Swap trigger | Alternatives |
|---|---|---|---|
| Open model backend | OpenRouter | Open model spend >$3-5K/mo | Fireworks (fastest), Together, DeepInfra (cheapest) |
| Frontier backend | OpenRouter | Want to eliminate middleman | Direct Anthropic/OpenAI API, AWS Bedrock |
| Model routing | Single-provider (OpenRouter) | Need provider-specific optimization | Multi-provider router in Djinn server |
| djinn-fast model | DeepSeek V3.2 | Better model available or quality issues | Qwen3 Coder, Llama 4, next-gen open model |
| djinn-pro model | MiniMax M2.5 | Better model or quality issues | Kimi K2, next-gen open model |
| GPU infra (Level 2) | None (not started) | Open model spend >$5K/mo | Modal Labs → SkyPilot → RunPod/Lambda |
| Billing proxy | OpenRouter per-key tracking | Fireworks direct ($3-5K/mo spend) | Add token counter (~100 lines Rust). See ADR-031 Part 8. |
| Billing proxy (full) | Credit system + OpenRouter | Enterprise customers or $50K+/mo spend | Full Rust metering + budgets + virtual keys (~800 lines). See ADR-031 Part 8. |
| Billing | Stripe | Need credits/wallets | Lago (OSS), Flexprice |
| Model serving (Level 2) | None (not started) | Level 2 triggered | vLLM, TensorRT-LLM, SGLang |

**The key invariant:** Customers interact through Djinn Cloud model names (`djinn-fast`, `djinn-pro`, `claude-opus`). Everything behind those names can change without customer impact.

---

## Success Metrics

### Launch (Week 2)

| Metric | Target |
|---|---|
| Signup → first agent task | <10 minutes for 80% of signups |
| djinn-fast tool call accuracy | >90% on Djinn's MCP tools |
| Free tier cost per user | <$1/month |

### Validation (Week 6)

| Metric | Target |
|---|---|
| Free → Pro conversion | >3% within 30 days |
| Pro MRR | $500+ |
| Metering accuracy | Within 5% of OpenRouter reported usage |

### 6-month targets

| Metric | Target |
|---|---|
| Djinn Cloud users | 500+ |
| Pro subscribers | 50+ |
| MRR | $2,000+ |
| Blended margin | >50% |

---

## Decision Log

| Date | Decision | Rationale |
|---|---|---|
| 2026-03-15 | Use OpenRouter as primary middleware | Per-customer keys, 300+ models, one integration. 5.5% fee is worth the simplicity. |
| 2026-03-15 | Fireworks AI as scale optimization (Month 3+) | Fastest open model inference. No middleman fee. Move open models here when spend justifies it. |
| 2026-03-15 | Reject LiteLLM | Billing bugs, memory leaks, 7 CVEs. OpenRouter gives us more with zero infra. |
| 2026-03-15 | Reject building custom billing proxy (800 lines Rust) | Too much effort for validation phase. OpenRouter handles routing and per-customer tracking. We only build credit system (~200 lines). |
| 2026-03-15 | DeepSeek V3.2 as djinn-fast | Cheapest coding model with full tool calling. 67-74% SWE-bench. |
| 2026-03-15 | MiniMax M2.5 as djinn-pro | 80.2% SWE-bench — highest open model. $1.20/MTok output. |
| 2026-03-15 | Launch with Free + Pro only | Two tiers. Learn before expanding. |
| 2026-03-15 | Level 2 (self-hosted) deferred until demand validates | No GPUs until open model spend exceeds $5K/mo through providers. |
| 2026-03-15 | Custom Rust billing proxy is viable — build incrementally | Not all at once. Token counter at Month 3 (Fireworks direct). Full metering at Month 6-12 (enterprise). Full proxy at Year 2 ($50K+/mo). See ADR-031 Part 8. |
