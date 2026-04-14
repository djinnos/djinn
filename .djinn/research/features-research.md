---
title: Features Research
type: research
tags: []
---

# AI Agent Orchestration Features Landscape (March 2026)

## Market Context
- 57% of companies run AI agents in production. 72% of enterprise AI projects use multi-agent architectures.
- Claude Code: ~4% of all public GitHub commits (~135k/day)
- Paradigm shift from "Conductor" (interactive single-agent) to "Orchestrator" (async multi-agent fire-and-forget)

## Competitive Landscape
- **Devin 2.0**: Cloud VMs, Slack-first, PR merge rate 67% (up from 34%), ACU pricing (~$20/mo entry)
- **Cursor 2.0**: Multi-agent orchestration, Planner/Worker/Judge, credit-based pricing
- **Windsurf**: Cascade (proactive agent), Flow Awareness, SWE-1.5 model, $15/seat unlimited
- **OpenAI Codex**: Cloud sandboxes, desktop app, GitHub integration, GPT-5.2-Codex
- **ComposioHQ Agent Orchestrator**: Open source, parallel agents in worktrees, CI auto-fix, 8 swappable plugins

## Table Stakes (Must Have)
1. **Git worktree isolation** per task — baseline infrastructure
2. **Task queue with dependency tracking** — structured dispatch
3. **Planner/Worker/Judge hierarchy** — proven pattern; flat agent topologies fail
4. **Async fire-and-forget with human gates** — audit trails required
5. **CI/CD integration** — auto-fix failures from CI logs
6. **Code review integration** — route PR comments to agents
7. **Observability/tracing** — 89% of prod teams require step-level tracing
8. **MCP support** — baseline tool connectivity standard
9. **Persistent cross-session memory** — CLAUDE.md/AGENTS.md/knowledge graphs
10. **Rollback/checkpointing** — git for code, orchestrator for agent state

## Differentiating (Djinn Advantages)
1. **Multi-model routing** — 76% of teams want it; premium models for planning, cheap for execution
2. **Circuit breakers per provider** — critical at scale, almost no orchestrators have it
3. **Tiered knowledge base** (3 tiers: hot/warm/cold) — 29% task runtime reduction
4. **Specialist-agent code review pipeline** — correctness/security/performance/standards agents
5. **Spec-driven acceptance criteria** — requirements agent validates before human review
6. **Attribution-based quality loop** — track finding acceptance rates to improve over time
7. **Compute governance / ACU budgets** — per-task compute caps
8. **License gating** — server-side validation, closed source server

## Pricing Models
| Model | Example | Verdict |
|---|---|---|
| Per-seat | Legacy Copilot | Losing ground |
| Per-token/credits | Cursor, Windsurf | Friction, self-censoring |
| Per-compute-unit | Devin ACUs | Best for orchestrator |
| Per-task-outcome | Cosine | Aspirational |

**Recommended**: Hybrid — base seat + compute-unit budgets per project + model-tier pricing.

## Key Benchmarks
- SWE-bench Verified (Jan 2026): Claude Opus 4.5 45.89%, Sonnet 4.5 43.60%, Gemini 3 Pro 43.30%
- AGENTS.md presence: 29% runtime reduction, 17% token reduction
- Google DORA 2025: 90% AI adoption = 9% more bugs, 154% larger PRs without quality controls

## Relations
- [[brief]] — project context
- [[Stack Research]] — technical stack for implementing features
- [[Architecture Research]] — patterns for building these features
- [[Pitfalls Research]] — risks in implementation