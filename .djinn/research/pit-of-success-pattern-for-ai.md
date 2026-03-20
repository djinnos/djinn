---
title: Pit of Success Pattern for AI
type: research
tags: ["architecture","ai-friendly","pit-of-success","design-constraints"]
---

## Source
- Blog: [Coding Horror — Falling Into the Pit of Success](https://blog.codinghorror.com/falling-into-the-pit-of-success/)
- Blog: [DEV.to — Preventing AI Agent Drift and Code Slop](https://dev.to/singhdevhub/how-we-prevent-ai-agents-drift-code-slop-generation-2eb7)
- Harvested: 2026-03-12

## Definition

> "We want our customers to simply fall into winning practices by using our platform and frameworks." — Rico Mariani

Design systems so the **easiest thing to write** is also the **correct thing**. The wrong approach should require active effort to pursue.

## Structural Pit-of-Success Patterns

**Typed errors make silent failure impossible:**
```rust
fn transition_task(id: TaskId, event: TaskEvent) -> Result<Task, TaskError>
// Not: fn transition_task(id: String, event: String) -> Option<Value>
```

**Repository traits constrain DB access:**
```rust
trait TaskRepo: Send + Sync {
    async fn find_by_id(&self, id: TaskId) -> Result<Option<Task>, RepoError>;
    async fn update_status(&self, id: TaskId, status: TaskStatus) -> Result<Task, RepoError>;
}
```

**Domain vocabulary in types:**
- `TaskId` (not `Uuid`), `WorktreePath` (not `PathBuf`), `SlotEvent::Free` (not `String`)

**Explicit completion contracts:**
- `complete_review` tool, `submit_plan` tool, `finish_verification` tool
- Creates auditable completion points instead of relying on natural language "I'm done"

## The AI Slop Problem

Without pit-of-success design, AI produces:
- Pattern contamination (reads legacy code, infers it as correct style)
- Open solution spaces (multiple valid approaches → picks one you didn't intend)
- Implicit conventions (anything not written down will be guessed wrongly)

## Prevention Mechanisms

- `.claudeignore` to exclude legacy/deprecated code from AI's view
- Reference file mapping pointing AI to exemplary files explicitly
- Curated context beats comprehensive context — trimming irrelevant sections improves output more than adding relevant ones

## Relations
- [[Type Systems as AI Architecture Guidance]]
- [[Hexagonal Architecture as AI Prompt]]
- [[AI Doom Loops and Naming as Infrastructure]]
