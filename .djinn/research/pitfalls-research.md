---
tags:
    - research
    - pitfalls
    - anti-patterns
    - rust
    - async
    - sqlite
    - rewrite
title: Pitfalls Research
type: research
---
# Critical Pitfalls for Djinn Server Rewrite (March 2026)

## 1. Go→Rust Async Traps

**Blocking in async context**: Go goroutines are preemptive; Tokio tasks are cooperative. Any sync I/O in async fn starves all workers. Use `spawn_blocking` for sync operations.

**Mutex deadlocks**: `std::sync::Mutex` across `.await` = deadlock. `tokio::sync::Mutex` across `.await` = silent state corruption on cancellation. Rule: use `std::sync::Mutex` without await, or use actor pattern.

**Async cancellation** (Oxide documented 4+ production bugs): `tokio::select!` drops losing futures mid-execution. Data loss from cancel-unsafe operations (e.g., `SinkExt::send`). Fix: pin futures outside loops, use scopeguard for cleanup, prefer actors over shared mutexes.

**`select!` loop recreation bug**: Future recreated each iteration loses buffered data. Pin and reuse futures.

**Second System Effect**: Resist implementing all deferred Go features at once. The old code's 32 migrations ARE documentation of production edge cases — treat as spec, not as complexity to avoid.

## 2. AI Agent Orchestrator Anti-Patterns

**Bag of Agents**: >4 unstructured agents = compound error rate drops to ~35%. Use topology-first design (Planner/Worker/Judge).

**Coordinator God Object**: Starts as thin router, accumulates business logic + state flags + retry logic + logging. Prevention: hard limit 20 fields per struct, decompose into actors with ≤15 message variants each.

**State Explosion**: Each boolean flag multiplies states exponentially. 6 flags = 64 states. Fix: use statecharts (hierarchical state machines) — parallel regions for independent variables.

**Poor failure taxonomy**: Classify failures up front — transient (retry), permanent (fail fast), ambiguous (escalate to human). Don't add retry logic reactively per incident.

## 3. libSQL/Turso Gotchas

**Data corruption during sync**: "Do not open the local database while the embedded replica is syncing." Unresolved as of early 2025.

**Embedded replicas are LEGACY**: Turso docs now recommend "Turso Sync" for new projects. Building on embedded replicas = immediate tech debt.

**4KB frame tax**: Every write consumes a full frame. High-frequency small writes (logs, heartbeats) are expensive for sync volume. Don't put high-frequency write tables in synced DB.

**Read-your-writes is per-replica only**: After server write, desktop won't see it until next `sync()`. Stale reads for display are OK; stale reads before acting are dangerous.

## 4. Single-DB Pitfalls

**Write serialization**: SQLite allows one writer at a time even in WAL. All domains (tasks, memory, settings, logs) share one write queue. Consider: separate DB files for high-write-frequency tables (logs, activity) vs main state.

**BUSY_SNAPSHOT**: Different from SQLITE_BUSY. `busy_timeout` doesn't help. Read transaction that wants to write on stale snapshot must retry ENTIRE transaction. Fix: use `BEGIN IMMEDIATE` for any transaction that writes.

**ALTER TABLE limitations**: Can't drop columns (pre-3.35), can't modify types, can't alter FKs. Requires 12-step table rebuild. ORMs often forget to recreate triggers/views during rebuild.

**Migration accumulation**: After 10 migrations, consolidate into canonical schema. Maintain committed `schema.sql`. Don't mix data migrations with schema migrations.

## 5. MCP Server Pitfalls

**stdout corruption**: Any println/logging to stdout corrupts JSON-RPC framing. ALL logging must go to stderr or file.

**Session state leaking**: Don't use global statics for per-session state. Each MCP session needs isolated state.

**SSE is deprecated**: Use Streamable HTTP. SSE has keepalive/timeout issues behind load balancers.

**Infinite tool call loops**: No spec-enforced call depth limit. Implement per-session max-calls counter.

## 6. Git Automation Pitfalls

**Stale worktree references**: `rm -rf` instead of `git worktree remove` leaves branch locked. Always run `git worktree prune` before creating new worktrees.

**Hook interactions**: Pre-commit hooks (husky, lint-staged) execute during automated commits. Can hang in non-TTY. WIP commits must use `--no-verify`. Agent working commits should let hooks run and fix failures. Coordinator merges should capture hook failures and re-dispatch agent.

**Concurrent git operations**: File locks (`.git/index.lock`). Serialize all git ops through a single GitActor per repository.

## 7. Licensing Pitfalls

**No trusted clock offline**: Clock-forward attacks bypass expiry trivially. Require online revalidation every N days. Log monotonic timestamps to detect clock rollback.

**JWT `alg: none`**: Never trust the token's `alg` field. Assert algorithm from config. Use Ed25519 (EdDSA) over RSA.

**No offline revocation**: Short expiry (30 days) forces periodic revalidation. Device fingerprinting limits sharing.

## 8. Scope Creep Prevention

**Warning signs from the Go server**:
- "Just add a column" as default response (instead of designing sub-entities)
- Nullable columns accumulating with unclear semantics
- Data migrations mixed with schema migrations
- No canonical schema.sql file
- Boolean flags for business logic (is_cancelled, is_paused, is_retrying)

**Prevention**:
- Schema review: any new column must justify why it's on the coordinator table vs. separate entity
- Canonical `schema.sql` committed alongside every migration
- Consolidate every 10 migrations
- Hard limit: coordinator struct >20 fields triggers mandatory decomposition
- Block features not in the Go server for first 6 months

## Relations
- [[brief]] — project context
- [[Stack Research]] — stack decisions informed by these pitfalls
- [[Architecture Research]] — architecture designed to avoid these pitfalls
- [[Features Research]] — feature scope risks