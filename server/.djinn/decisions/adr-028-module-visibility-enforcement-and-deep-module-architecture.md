---
title: ADR-028: Module Visibility Enforcement and Deep Module Architecture
type: adr
tags: ["adr","architecture","modules","linting","rust","deep-modules"]
---

# ADR-028: Module Visibility Enforcement and Deep Module Architecture

## Status: Accepted

Date: 2026-03-12

## Context

An architecture audit of the server codebase revealed several structural issues that impede maintainability and increase the risk of accidental API surface expansion:

1. **No visibility enforcement** — `lib.rs` declares all 16 top-level modules as `pub` with no compiler lint to catch over-exposed internals. Items marked `pub` inside private submodules are unreachable from outside the crate but the compiler won't warn about this by default.

2. **Deep import paths without facades** — consumers reach 4 segments deep into internal structure: `crate::db::repositories::task::TaskRepository`. A refactor inside `db/repositories/task/` forces changes across 20+ files.

3. **Agent module leaks internals** — `agent::compaction`, `agent::output_parser`, and `agent::extension` are all `pub` but are implementation details never consumed outside the crate.

4. **Cross-boundary coupling** — `agent::extension` imports from `actors::slot::task_review` (`merge_and_transition`, `PM_MERGE_ACTIONS`), creating a bidirectional dependency between two unrelated module hierarchies.

5. **No static analysis tooling** — module structure and dependency graphs were only discoverable by manual code reading.

### Audit Metrics (2026-03-12 baseline)

- **456 tests**, all passing, ~12s runtime
- **47.26% line coverage** (6,107 / 12,922 lines via cargo-tarpaulin)
- **9 `unreachable_pub` violations** found when lint was enabled — all in 3 files
- **Biggest coverage gaps**: `watchers/kb.rs` (0%), `server/chat.rs` (17%), `server/state/mod.rs` (26%), `models/task.rs` (64% — only 3 state machine tests)

## Decision

### 1. Enable `#![warn(unreachable_pub)]` in `lib.rs`

This Rust compiler lint flags any `pub` item that isn't reachable from the crate root. Combined with the existing `-D warnings` in verification commands, this makes over-exposed internals a compile error.

**Rationale**: The lint is zero-cost, catches exactly the problem we have, and requires no runtime changes. It acts as a guardrail — new code that accidentally uses `pub` where `pub(crate)` or `pub(super)` is correct will fail CI.

**Blast radius**: Only 9 items needed fixing across 3 files (`actors/slot/commands.rs`, `actors/slot/pool/`, `server/chat.rs`). All were trivial `pub` → `pub(super)` or `pub(crate)` changes.

### 2. Install `cargo-modules` for Static Analysis

`cargo-modules` provides two commands:
- `cargo modules structure --lib` — prints the full module tree with visibility annotations (pub, pub(crate), pub(super), private)
- `cargo modules dependencies --lib` — generates a DOT-format dependency graph showing which modules depend on which

**Rationale**: Manual code reading doesn't scale for ongoing enforcement. `cargo-modules` makes the module tree and cross-boundary dependencies visible at a glance. It should be run during architecture reviews and before major refactors.

**Usage pattern**: Not a CI gate (too noisy), but a developer tool for architecture validation. Run `cargo modules structure --lib` to verify visibility, `cargo modules dependencies --lib --no-externs` to check for unexpected cross-module dependencies.

### 3. Facade Re-exports on `db/` and `models/`

Add `pub use` re-exports so consumers write:
```rust
use crate::db::TaskRepository;      // not crate::db::repositories::task::TaskRepository
use crate::models::Task;             // not crate::models::task::Task
```

The internal module structure remains unchanged — facades just flatten the public API surface. This is the "deep module" pattern: a simple interface hiding complex internals.

### 4. `pub(crate)` Sweep on Agent Internals

Mark `agent::compaction`, `agent::output_parser`, and `agent::extension` as `pub(crate)`. Re-export only what external consumers need from `agent/mod.rs`.

### 5. Extract Shared Task Transition Types

Move `merge_and_transition` and `PM_MERGE_ACTIONS` out of `actors::slot::task_review` into a neutral location (likely `models::task` or `db::repositories::task`), breaking the cross-boundary coupling between `agent` and `actors`.

## Consequences

### Positive
- **Compiler-enforced visibility** — future PRs cannot accidentally widen the API surface
- **Shorter import paths** — facade re-exports reduce cognitive load and diff noise during refactors
- **Architecture visibility** — `cargo-modules` makes structure decisions auditable
- **Decoupled modules** — extracting shared types removes the bidirectional agent↔actor dependency
- **Rust project template** — `unreachable_pub` + facade pattern can be adopted as default for new Djinn-managed Rust projects

### Negative
- **Dual import paths during migration** — until all consumers migrate to facade paths, both work. This is temporary.
- **`cargo-modules` is a dev dependency** — must be installed per-developer, not enforced by CI

### Neutral
- Test count unchanged — these are structural changes enforced by the compiler, not behavioral changes requiring new tests
- The `unreachable_pub` lint only applies to library code (items inside `src/lib.rs` module tree), not binary targets

## References

- [[Deep Modules Pattern for AI Codebases]]
- [[Pit of Success Pattern for AI]]
- [[Rust Architecture and Boundary Enforcement Tools]]
- [[Type Systems as AI Architecture Guidance]]
- [[ADR-026: Automated Testing Strategy]]