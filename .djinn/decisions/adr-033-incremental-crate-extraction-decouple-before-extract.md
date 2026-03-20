---
title: ADR-033: Incremental Crate Extraction — Decouple Before Extract
type: adr
tags: ["adr","architecture","workspace","cargo","migration","events","repositories"]
---

# ADR-033: Incremental Crate Extraction — Decouple Before Extract

## Status: Accepted

Date: 2026-03-16

Supersedes: ADR-029 (workspace split portion only; role trait portion remains Draft)

## Context

### Failed Attempts

ADR-029 proposed splitting the monolith into 6 vertical workspace crates. Two epic-scale attempts failed:

- **Epic `vqc5` (Vertical Workspace Split)**: 53 tasks, mostly force-closed. Approach was wrong — tried to extract crates before decoupling internal dependencies.
- **Epic `6rcy` (Agent Role Trait)**: 35 tasks, foundation landed but migration stuck at coordinator/lifecycle integration boundary.
- **DjinnEvent flat-struct replacement**: Reverted within 10 minutes (commit `e638172` reverting `026be56`) — consumers broke immediately because the 29-variant enum imports types from every domain.
- **Model movement sessions**: Multiple agent sessions on 2026-03-16 produced 40+ "WIP: interrupted session" commits with no successful extraction.

### Root Cause: Three Coupling Points

1. **DjinnEvent enum (29 variants)** imports `Task`, `Epic`, `Project`, `Note`, `Credential`, `GitSettings`, `StepEvent`, `ProjectConfig`. Moving it to any crate creates circular dependencies because every domain type must follow.

2. **`broadcast::Sender<DjinnEventEnvelope>`** is a concrete tokio type baked into all 10 repository constructors (39 send sites across 16 files). Repos cannot move to another crate without dragging the tokio broadcast dependency.

3. **4 duplicate model definitions** in `src/models/` (epic, project, credential, provider) are inline copies instead of re-exports from djinn-core. Model changes require manual sync.

### Key Insight

**Decouple before extracting.** The previous approach tried to move code to new crates while dependencies still pointed inward. This ADR defines 4 sequential phases that remove coupling points first, making the final extraction mechanical.

## Decision

### Phase 1: Envelope-Only Events

**Kill the DjinnEvent enum.** It exists solely to be converted into `DjinnEventEnvelope` via `From<DjinnEvent>`. The envelope already carries `(entity_type, action, payload)` as `(&'static str, &'static str, serde_json::Value)` — zero type dependencies.

Steps:
1. Add typed constructor methods on `DjinnEventEnvelope` (e.g., `::task_created(task, from_sync)`, `::epic_updated(epic)`) — one per current variant
2. Replace all 39 `self.events.send(DjinnEvent::Foo(...).into())` with direct `DjinnEventEnvelope::foo(...)` calls
3. Delete `DjinnEvent` enum, the `From` impl, and update tests
4. Move `DjinnEventEnvelope` + constructors to djinn-core (they reference only model types already in djinn-core)

### Phase 2: Consolidate Model Re-exports

Eliminate the 4 inline model copies in `src/models/`.

Steps:
1. Ensure djinn-core models derive `sqlx::FromRow` behind the `sqlx` feature flag (epic, project, credential, provider, custom_provider)
2. Replace inline copies with `pub use djinn_core::models::*` re-exports
3. Delete `src/models/` module — re-export directly from `lib.rs`

### Phase 3: EventBus Abstraction

Decouple repositories from the concrete `tokio::sync::broadcast::Sender` type.

Steps:
1. Define `EventBus` newtype in djinn-core: a simple callback wrapper `pub struct EventBus(Box<dyn Fn(DjinnEventEnvelope) + Send + Sync>)`
2. Update all 10 repo constructors: `new(db: Database, events: EventBus)` instead of raw broadcast sender
3. `AppState` constructs `EventBus` from its broadcast sender at initialization

### Phase 4: Move Repositories to djinn-db

With phases 1-3 complete, repositories depend on:
- `Database` (already in djinn-db)
- Model types (djinn-core)
- `EventBus` (djinn-core)

No circular dependencies possible. The dependency graph becomes:

```
djinn-core  (models, state machine, EventBus, DjinnEventEnvelope)
    ↑
djinn-db    (Database, migrations, ALL repositories)
    ↑
djinn-server (actors, agents, MCP, HTTP, SSE)
```

Steps:
1. Move simple repos first (credential, settings, git_settings, custom_provider) — no inter-repo deps
2. Move medium repos (epic, project, session, session_message)
3. Move complex repos (task — has blockers/activity/transitions/status sub-modules)
4. Move note repo (has indexing, search, graph sub-modules)

## Consequences

### Positive
- Each phase is independently valuable — no wasted work if later phases are deferred
- Phase 1 alone removes the worst dependency knot (DjinnEvent → every model type)
- Phase 4 is mechanical after phases 1-3 — no architectural decisions needed
- ~12 tasks total across 4 epics (vs 88 tasks in the failed ADR-029 attempt)
- Each task is small and self-contained — no cross-cutting changes

### Negative
- Phase 3 adds a layer of indirection (EventBus callback vs direct broadcast send)
- Phase 1 loses compile-time exhaustiveness checking on event variants (envelope is stringly-typed)
- Must complete phases 1-3 before phase 4 — sequential dependency

### Mitigations
- Envelope constructors are typed methods — wrong field types still caught at compile time
- Unit tests cover all envelope entity_type/action combinations (already exist)
- EventBus is a thin newtype — zero runtime cost vs broadcast send

## Relations

- [[ADR-029: Vertical Workspace Splitting and Agent Role Trait]] — superseded (workspace portion)
- [[ADR-028: Module Visibility Enforcement and Deep Module Architecture]] — complementary