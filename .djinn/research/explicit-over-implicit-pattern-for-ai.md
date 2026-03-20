---
title: Explicit Over Implicit Pattern for AI
type: research
tags: ["architecture","ai-friendly","explicit","anti-metaprogramming","context-engineering"]
---

## Source
- Blog: [Martin Fowler — Context Engineering for Coding Agents](https://martinfowler.com/articles/exploring-gen-ai/context-engineering-coding-agents.html)
- Blog: [Pete Hodgson — Why Your AI Coding Assistant Keeps Doing It Wrong](https://blog.thepete.net/blog/2025/05/22/why-your-ai-coding-assistant-keeps-doing-it-wrong-and-how-to-fix-it/)
- Multiple HN threads
- Harvested: 2026-03-12

## Core Principle

> "Every session starts from scratch. Every invocation pays the full cost of whatever wasn't made explicit in the code."

When agents encounter ambiguity, they infer. Sometimes those inferences compile, pass review, and get shipped.

## Implicit vs Explicit

| Implicit | Explicit |
|----------|----------|
| "We always use UUIDv7" (tribal knowledge) | Codified in CLAUDE.md + type alias `EntityId = Uuid` |
| "Tests don't use async directly" | `#[cfg(test)]` helper that wraps tokio |
| "All writes go through repo layer" | Trait + compiler enforcement |
| Feature groupings in your head | Reflected in file system |

## Anti-Metaprogramming

Metaprogramming, dynamic dispatch via string-matching, reflection-based autowiring, monkey-patching — all **invisible to AI**. If AI needs to *run* the code to understand what it does, the code is too implicit.

Concrete anti-patterns:
- Auto-wiring via reflection or naming conventions
- Plugin systems with implicit registration
- `any` types that hide actual contracts
- Scattered environment variables (consolidate into one config module)

## Constraint-Context Matrix (Pete Hodgson)

Two dimensions determine AI task suitability:
- **Solution space**: Constrained (one correct path) vs. Open (many approaches)
- **Context**: Provided (self-contained) vs. Implied (requires codebase knowledge)

AI excels at **constrained + provided**. Design modules so most tasks fall in that quadrant.

## Context Specification Costs

| Approach | Token cost |
|----------|-----------|
| Naming 3 files explicitly for the agent | ~100 tokens |
| Letting agent search the codebase | 1,000-10,000 tokens |

**Removing irrelevant context improves performance more than adding relevant context.**

## Relations
- [[AI Doom Loops and Naming as Infrastructure]]
- [[Codified Context Three-Tier Architecture]]
- [[Deep Modules Pattern for AI Codebases]]
