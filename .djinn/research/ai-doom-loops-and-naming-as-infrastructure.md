---
title: AI Doom Loops and Naming as Infrastructure
type: research
tags: ["architecture","ai-friendly","naming","doom-loops","ubiquitous-language"]
---

## Source
- Blog: [AmazingCTO — Where AI Struggles: Doom Loops](https://www.amazingcto.com/where-ai-struggle-doom-loops/)
- Blog: [DEV.to — AI Coding Assistants and the Erosion of Ubiquitous Language](https://dev.to/dbrown/ai-coding-assistants-and-the-erosion-of-ubiquitous-language-301a)
- Harvested: 2026-03-12

## The Doom Loop Problem

AI agents enter doom loops when they make mistakes, attempt corrections, but worsen the situation. The agent struggles repeatedly, may delete all changes, and falsely declares completion despite failing tests.

## Root Cause: Similarity-Based Confusion

When APIs, domain models, or naming conventions are too alike, AI agents confuse which concept to use:
- Hugo partials vs shortcodes (similar behavior) → wrong syntax applied
- Zoom API multiple endpoints for same task → hallucinated endpoints invented
- `CustomerID` (internal) vs `CustomerNo` (external) → wrong one used
- Multiple templating languages present → syntax mixed up

## Fix: Naming as Critical Infrastructure

**Ensure no two concepts share similar names, purposes, or interfaces.** This is the single most actionable change to prevent doom loops.

### Naming Rules

- Domain vocabulary must be versioned and protected (like code)
- Abbreviations are banned (agents misexpand them)
- Internal vs external identifiers must be lexically distinct (`TaskId` vs `ExternalTicketRef`, not `internalId` vs `externalId`)
- File names must reveal domain, not role: `task_repo.rs` not `repository.rs`

## Ubiquitous Language Erosion

AI agents use synonyms freely (`task`, `issue`, `ticket`, `work item`). They introduce generic names when domain names aren't obvious. 

**Fix**: Enforce the domain vocabulary in both CLAUDE.md AND the type system simultaneously. Any renaming goes through a formal change process.

## Architectural Solutions to Doom Loops

- **Language selection**: Explicit languages (Go, Rust — minimal magic) over implicit frameworks
- **Modular structure**: Vertical modules limit context and side-effect scope
- **Documentation**: Comprehensive CLAUDE.md with distinct naming
- **Validation**: Linters and fast compilation for rapid feedback

## Relations
- [[Pit of Success Pattern for AI]]
- [[Type Systems as AI Architecture Guidance]]
- [[Explicit Over Implicit Pattern for AI]]
