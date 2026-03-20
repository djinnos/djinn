---
title: Codified Context Three-Tier Architecture
type: research
tags: ["architecture","ai-friendly","context-engineering","claude-md","agents-md"]
---

## Source
- Paper: [arXiv 2602.20478 — Codified Context: Infrastructure for AI Agents in a Complex Codebase](https://arxiv.org/html/2602.20478v1) — validated at 108K LOC, 283 sessions, 70 days
- Blog: [Martin Fowler — Context Engineering for Coding Agents](https://martinfowler.com/articles/exploring-gen-ai/context-engineering-coding-agents.html)
- Blog: [Anthropic — Effective Context Engineering for AI Agents](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
- Harvested: 2026-03-12

## The Problem

Single-file CLAUDE.md / AGENTS.md does not scale beyond ~100K LOC. Context is infrastructure, not an afterthought. Bigger context is not better context — context rot degrades model recall and reasoning.

## Three-Tier Architecture

| Tier | Content | Size | Loading |
|------|---------|------|---------|
| **Hot (Tier 1)** | Conventions, naming standards, architecture invariants, routing table | ~660 lines | Always loaded |
| **Domain (Tier 2)** | Per-domain agent specs (>50% is domain knowledge, not behavioral) | ~9,300 lines across 19 agents | Per-task |
| **Cold (Tier 3)** | Subsystem reference docs, machine-readable | ~16,250 lines, 34 docs | On-demand via MCP |

**Knowledge-to-code ratio**: 26,200 lines of context infrastructure for 108,000 lines of code = **24.2%**

## Key Findings

- **Brevity bias**: Iterative optimization collapses prompts toward generic; domain knowledge must be explicitly embedded. Over half of each specialist spec's content is project-domain knowledge.
- **Trigger tables**: Route tasks to appropriate specialists based on file patterns being modified.
- **80%+ of prompts were ≤100 words** when pre-loaded context is good.
- **Stale specs are worse than no specs**: Agents trust documentation absolutely — outdated specs cause silent failures.
- Maintenance cost: 1-2 hours/week, integrated into dev sessions.

## Guidelines (G1-G6 from paper)

1. Start with a minimal constitution early — even basic tech stack info dramatically improves output
2. Activate planning agents to surface required specs before implementation
3. Document repeated explanations as specs — repetition signals codification need
4. Treat specs as load-bearing infrastructure — staleness is dangerous
5. Create specialized agents when unguided sessions stall repeatedly in specific domains
6. Monitor agent confusion as diagnostic signal for missing/outdated specs

## CLAUDE.md Best Practices

**Content hierarchy (highest to lowest value):**
1. Non-obvious invariants and constraints (tribal knowledge)
2. Exact command invocations with correct flags
3. Module entry points and ownership boundaries
4. "Why we didn't do X" decision rationale
5. Known gotchas from past agent failures

**Budget**: Frontier LLMs reliably follow 150-200 instructions. Claude Code's built-in system prompt consumes ~50 slots. Your CLAUDE.md budget: ~100-150 instructions max. Target under 300 lines, ideally under 60.

**Nested strategy:**
```
src/
  CLAUDE.md             ← module entry points, cross-cutting constraints
  actors/
    CLAUDE.md           ← actor message patterns, supervision rules
  agent/
    CLAUDE.md           ← LLM provider abstraction, prompt conventions
```

**Anti-patterns**: LLM-generated context docs (-3% performance), oversized files, absolute prohibitions, comprehensive onboarding manuals.

## Context Window Management

| Trigger | Action |
|---------|--------|
| ~40% context used | Consider scoping narrower |
| ~60% threshold | Clear + resume from saved progress |
| Task boundary | Git commit + fresh session |

**Explicit context specification beats automatic discovery.** Naming 3 files costs ~100 tokens. Letting the agent search costs 1,000-10,000. Removing irrelevant context improves performance more than adding relevant context.

## Relations
- [[CLAUDE.md and Instruction File Patterns]]
- [[Deep Modules Pattern for AI Codebases]]
- [[Vertical Slice Architecture for AI]]
