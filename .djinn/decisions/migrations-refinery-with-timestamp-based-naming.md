---
title: Migrations — refinery with timestamp-based naming
type: adr
tags: []
---

# ADR-003: Migrations — refinery with timestamp-based naming

## Status: Accepted

## Context

The original plan called for hand-rolled migrations with `include_str!` because neither sqlx nor refinery supported async libsql. ADR-002 switched to rusqlite, which refinery supports directly.

The Go server used goose for migrations. A known pitfall: AI agents writing code would create migrations with incorrect sequence numbers (e.g., writing migration 3 when migration 20 already exists), causing ordering violations and schema corruption.

## Decision

Use **refinery 0.9** with the `int8-versions` feature for timestamp-based migration naming.

```toml
refinery = { version = "0.9", features = ["rusqlite", "int8-versions"] }
```

### Naming convention

Migrations use timestamp-derived version numbers: `V{YYYYMMDDHHMMSS}__{description}.sql`

```
migrations/
  V20260302000001__create_tasks.sql
  V20260302000002__create_epics.sql
  V20260303000001__create_notes.sql
```

The `int8-versions` feature allows i64 version numbers, which fits full timestamps. This MUST be enabled before the first migration runs — it cannot be retrofitted.

### Why this prevents the AI ordering problem

- Version numbers are timestamps, not sequential integers — no "what's the next number?" question
- Two developers (or AI agents) on different branches get different timestamps — no collision
- refinery with `V` prefix enforces ascending order and detects gaps
- Applied migrations are immutable (checksum-verified) — changing a file after applying breaks the build

### Migration rules

1. Never edit an applied migration — write a new one to fix/undo
2. Maintain a canonical `schema.sql` alongside migrations (the ground truth)
3. After 10 migrations, consolidate into a new canonical base + fresh migration series
4. `int8-versions` is a day-one decision — enabled from the very first migration

## Consequences

### Positive
- AI agents cannot create out-of-order migrations (timestamps are always ascending)
- Gap detection catches missing migrations from branch merges
- Checksum verification catches accidental edits to applied migrations
- Zero custom migration code — refinery handles everything

### Negative
- `int8-versions` must be enabled from day one (cannot retrofit)
- refinery 0.9 tested against rusqlite 0.37 — pin to 0.37 for safety until refinery 0.9.1+ confirms 0.38

### Supersedes
- Stack Research recommendation for hand-rolled migrations with `include_str!` (that was for libsql; we now use rusqlite)

## Relations
- [[Database Layer — rusqlite over libsql/Turso]] — ADR-002 enabled this by switching to rusqlite
- [[requirements/v1-requirements]] — updates DB-04
- [[Stack Research]] — original migration recommendation superseded