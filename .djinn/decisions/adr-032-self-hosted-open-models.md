---
title: "ADR-032: Self-Hosted Open Models via Managed GPU (Level 2 — Future)"
type: adr
tags: ["adr", "inference", "self-hosted", "open-models", "gpu", "monetization", "djinn-cloud", "modal", "vllm"]
---

# ADR-032: Self-Hosted Open Models via Managed GPU (Level 2)

## Status: Deferred (pursue only after ADR-031 validates demand)

Date: 2026-03-15 (revised)

## Related

- [[ADR-031]]: Djinn Cloud — Middleware Integration — **prerequisite, must ship and validate first**
- [[Djinn Cloud Strategy Overview]]

## Context

### This ADR is NOT the current plan

ADR-031 (OpenRouter + Fireworks middleware) is the current plan. This ADR documents the NEXT step — self-hosting models on rented GPUs — which we pursue only after Level 1 validates demand.

**Trigger to begin Level 2:** Open model spend through OpenRouter/Fireworks exceeds $5K/month.

### Why self-host eventually?

| | Level 1 (middleware) | Level 2 (self-hosted) |
|---|---|---|
| djinn-fast cost | ~$0.38/MTok (OpenRouter) | ~$0.02/MTok (vLLM on L40S) |
| djinn-pro cost | ~$1.20/MTok (OpenRouter) | ~$0.08/MTok (vLLM on H100) |
| Margin on djinn-fast | ~67% | ~90% |
| Margin blended | ~50% | ~75% |
| Fine-tuning | Not possible | Yes — specialize for Djinn's tools |
| Infra ops | Zero | ~8 hrs/week |

The margin jump from 50% → 75% is the difference between a lifestyle business and a venture-scale business. But it only matters at volume.

### What "self-hosted" means here

**Level 2 is NOT Level 3.** We don't build Fireworks. We don't buy GPUs. We don't manage Kubernetes.

We rent GPUs from Modal Labs (serverless, scales to zero, no K8s) and deploy vLLM (open-source model server). Modal manages the machine. We manage the model.

```
Level 1: Djinn Server → OpenRouter API → (Fireworks serves the model)
Level 2: Djinn Server → Modal Labs → vLLM (we serve the model on Modal's GPU)
Level 3: Djinn Server → Our GPU cluster → vLLM (we own everything) ← NEVER
```

## Decision (deferred — documenting the plan for when triggered)

### Part 1: Deploy vLLM on Modal Labs

```python
import modal

app = modal.App("djinn-inference")

@app.cls(gpu=modal.gpu.L40S(), container_idle_timeout=300)
class DjinnFast:
    @modal.enter()
    def load_model(self):
        from vllm import LLM
        self.llm = LLM(
            model="Qwen/Qwen3-8B",  # or current djinn-fast model
            trust_remote_code=True,
            max_model_len=32768,
            gpu_memory_utilization=0.90,
        )

    @modal.method()
    def generate(self, messages, tools, **kwargs):
        # OpenAI-compatible chat completion
        ...
```

**GPU selection:**
- djinn-fast (8B model): L40S ($0.40-0.87/hr) — 48GB, plenty for 8B + KV cache
- djinn-pro (larger model): H100 ($1.50-2.50/hr) — needed for MoE models

### Part 2: Fine-tune on Djinn's tool schema

1. Collect ~1,000 examples of Djinn MCP tool calls from real agent sessions
2. Fine-tune with LoRA (rank 16, alpha 32) — 2-4 hours on a single H100
3. The fine-tuned model is specifically better at Djinn's 50+ tools
4. This creates a genuine moat — our model is better for Djinn tasks than any generic model

### Part 3: Update model router

Change `resolve_model()` to route open models to Modal instead of OpenRouter/Fireworks:

```rust
"djinn-fast" => UpstreamTarget {
    base_url: "https://djinn-inference--djinn-fast.modal.run/v1",
    model_id: "djinn-fast-v1",
    auth: AuthMethod::BearerToken(our_modal_key),
},
```

Frontier models (Claude, GPT-4o) continue through OpenRouter or direct API. The customer sees no change.

### Part 4: Add token counting

With self-hosted models, we lose OpenRouter's per-key usage tracking. We add our own (~100 lines of Rust): extract token counts from vLLM's response and log to `credit_ledger`.

## When to trigger

| Metric | Threshold | Action |
|---|---|---|
| Open model spend (OpenRouter/Fireworks) | >$5K/month | Begin Level 2 evaluation |
| GPU utilization (if already on Level 2) | >70% sustained | Switch from Modal serverless to reserved capacity |
| GPU spend | >$5K/month | Evaluate SkyPilot for multi-cloud optimization |
| Task completion rate (self-hosted) | <95% of middleware baseline | Roll back, investigate |

## Testing Plan (when triggered)

1. **Model eval** — Run Djinn's tool-call test suite against self-hosted model. Must pass >90% accuracy.
2. **Shadow mode** — Route tasks to both middleware AND self-hosted. Compare quality. Don't serve self-hosted to customers yet.
3. **Canary 10%** — Route 10% of simple tasks to self-hosted. Monitor completion rate.
4. **Gradual ramp** — 10% → 25% → 50% → 100%. Roll back if quality drops >5%.

## GPU Provider Options (evaluate when triggered)

| Provider | Why | When |
|---|---|---|
| **Modal Labs** | Serverless, scales to zero, $25K startup credits | First choice |
| **SkyPilot** | Multi-cloud orchestrator, auto-picks cheapest GPU | When GPU spend >$5K/mo |
| **RunPod** | Cheap, Docker-based, secure cloud option | Alternative to Modal |
| **Lambda Labs** | Reserved H100s, simple | When need fixed capacity |

## Relations

- [[ADR-031]]: Djinn Cloud — Middleware Integration
- [[Djinn Cloud Strategy Overview]]
