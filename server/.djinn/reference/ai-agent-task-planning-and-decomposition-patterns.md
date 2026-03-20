---
title: AI Agent Task Planning and Decomposition Patterns
type: reference
tags: []
---

# AI Agent Task Planning and Decomposition Patterns

Research from 2026-03-13. Covers Augment Intent, Kiro, Devin, Factory.ai, SWE-agent, Agent Teams Lite, Anthropic Multi-Agent.

## Key Patterns That Prevent Runaway Decomposition

1. **Spec-First Gate** (Augment Intent, Kiro) — human-approved spec before implementation
2. **Tasks Trace to Spec** (Kiro) — Requirements → Design → Tasks pipeline
3. **Effort Calibration Rules** (Anthropic, Agent Teams Lite) — "3-8 tasks per phase" in prompt
4. **Codebase Graph** (Factory.ai) — file dependency graph before decomposition
5. **Isolation + Named Merge Point** — parallel worktrees, human-gated integration
6. **Verifier as Separate Role** — implementer cannot self-verify
7. **Checkpoint at Every Completion** — git commit before next mutation

## Root Causes of PM Task Explosion

- No spec gate — agents decompose from vague English
- PM sees only failing task — no epic, no sibling tasks, no ADRs
- No effort calibration — no "stop decomposing" signal
- No codebase dependency graph — misses cross-cutting impacts

## Applied Fixes (Epic 4ykv — Agent Context Enrichment)

1. Add memory_refs to Epic model (link ADRs/specs to epics)
2. Groomer reviews epic quality before grooming tasks
3. PM prompt injects epic context (description, memory_refs, siblings)
4. PM gets decomposition rules (max 4 subtasks, read epic first)

## Key Sources

- Augment Code: augmentcode.com/blog/intent-a-workspace-for-agent-orchestration
- Kiro (AWS): kiro.dev/blog/introducing-kiro
- Devin: cognition.ai/blog/devin-annual-performance-review-2025
- Factory.ai: zenml.io/llmops-database (HyperCode analysis)
- Agent Teams Lite: github.com/Gentleman-Programming/agent-teams-lite
- Anthropic Multi-Agent: anthropic.com/engineering/multi-agent-research-system
