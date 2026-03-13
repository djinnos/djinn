---
title: Type Systems as AI Architecture Guidance
type: research
tags: ["architecture","ai-friendly","type-systems","rust","compiler-enforcement"]
---

## Source
- Blog: [Ben Houston — Agentic Coding Best Practices](https://benhouston3d.com/blog/agentic-coding-best-practices)
- Blog: [DEV.to — I Made My Code Dumber](https://dev.to/matthewhou/i-stopped-trying-to-make-ai-smarter-i-made-my-code-dumber-4npa)
- Harvested: 2026-03-12

## Core Principle

Strong types constrain the solution space **without consuming prompt budget**. The agent cannot return the wrong thing if the return type is `Result<TaskId, TaskError>` rather than `Value` or `HashMap<String, Any>`.

## Quantitative Impact

Shifting from runtime convention-based patterns to compile-time type enforcement improved one project's AI success from **~60% to ~100%**.

## Type System Hierarchy for AI Friendliness

| Language | AI Benefit | AI Risk |
|----------|-----------|---------|
| Rust | Type system enforces correctness, compiler catches agent mistakes, no GC | Lifetime/async complexity causes agent thrash |
| TypeScript (strict) | Structural types constrain solution space | `any` escape hatch, agents will use it |
| Python (typed) | Gradual types, agents ignore them if not enforced | Agents produce untyped code unless rules require it |
| JavaScript | No guardrails | Agents write anything; all looks equally valid |

## Rust-Specific Considerations

The Rust compiler acts as a second reviewer. An agent that writes wrong ownership gets a compile error, not a runtime bug shipped to production.

**Caveat**: With async code in Rust, lifetimes can cause AI agent thrash requiring manual intervention. Mitigation: document which async patterns to use in CLAUDE.md. Prefer `Arc<Mutex<T>>` over complex lifetime solutions.

## Domain Vocabulary in Types

Types prevent vocabulary drift:
- `TaskId` not `Uuid` — agents can't mix up entity types
- `WorktreePath` not `PathBuf` — domain meaning is encoded
- `SlotEvent::Free` not `String` — invalid states unrepresentable
- `Result<Task, TaskError>` not `Option<Value>` — silent failure impossible

## Practical Rules

- Use strict typing everywhere — function returns, variables, collections
- Avoid generic types (`Any`, `unknown`, `List[Dict[str, Any]]`)
- Enforce explicit parameters over defaults
- Favor functional programming; reserve OOP for external system connectors
- Write pure functions without side effects on inputs or globals
- Use specific error types with actionable messages

## Relations
- [[Pit of Success Pattern for AI]]
- [[Hexagonal Architecture as AI Prompt]]
- [[Deep Modules Pattern for AI Codebases]]
