---
title: Vertical Slice Architecture for AI
type: research
tags: ["architecture","ai-friendly","vertical-slice","feature-modules"]
---

## Source
- Forum: [Cursor — VSA for Complex Projects](https://forum.cursor.com/t/vsa-vertical-slice-architecture-for-complex-projects/75360)
- Blog: [DEV.to — Coding Agents as First-Class Architecture](https://dev.to/somedood/coding-agents-as-a-first-class-consideration-in-project-structures-2a6b)
- Blog: [LogRocket — AI-Ready Frontend Architecture](https://blog.logrocket.com/ai-ready-frontend-architecture-guide/)
- Harvested: 2026-03-12

## Definition

Code organized by **feature** (vertical slice through all layers) rather than by technical layer (all controllers together, all services together). Each feature folder contains its own handler, service, repository, types, and tests.

## The 40% Rule

LLM effectiveness degrades significantly past 40% of context window capacity. Horizontal architectures scatter a single feature across multiple directories, forcing agents to traverse widely before reasoning about a concern.

## Structure

```
# BAD: horizontal                    # GOOD: vertical
src/                                  src/features/
  controllers/                          auth/
    auth_controller.rs                    handler.rs
    billing_controller.rs                 service.rs
  services/                               repo.rs
    auth_service.rs                       tests.rs
    billing_service.rs                  billing/
  repos/                                  handler.rs
    auth_repo.rs                          service.rs
    billing_repo.rs                       repo.rs
                                          tests.rs
```

## Benefits for AI Agents

- Fewer directory jumps per task — entire feature loads within context
- Change isolation — modifications stay within the slice
- New features = new files, minimal edits elsewhere
- Concurrent agents working in isolation with low merge conflict risk
- Mirrors business capabilities so AI can reason about user intent

## Anti-Pattern: Monolithic Service Classes

A monolithic `UserService` that bundles unrelated operations forces an agent trying to add billing to read all of `UserService` because it lives in the same file.

## Trade-offs

- Requires skilled developers for ongoing refactoring; without discipline, slices degrade
- Some cross-cutting concerns (auth middleware, logging) still need shared infrastructure
- Not all code is feature-specific — shared types, utilities need a home

## AI Productivity by Codebase Size

| Size | AI productivity multiplier |
|------|---------------------------|
| 10K lines | 10x |
| 100K lines | 2-3x |
| 1M+ lines | 1-1.5x |

Vertical slicing is the primary lever against this dropoff — keeps effective context per-task small regardless of total codebase size.

## Relations
- [[Deep Modules Pattern for AI Codebases]]
- [[Codified Context Three-Tier Architecture]]
- [[Agent Parallelism and Structural Isolation]]
