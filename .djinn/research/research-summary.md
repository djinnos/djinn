---
tags:
    - research
    - synthesis
title: Research Summary
type: research
---
# Research Synthesis — Djinn Server Rust Rewrite

Cross-cutting analysis of Stack, Features, Architecture, and Pitfalls research, plus supplementary database investigation.

## Convergent Themes

### 1. Simplicity Is the Architecture

Every research dimension independently arrived at the same conclusion: the biggest risk to this project is accumulated complexity, and the strongest mitigation is aggressive simplicity.

- **Stack**: rusqlite + WAL over Turso/libsql — proven, zero-plumbing cross-process reads ([[Database Layer — rusqlite over libsql/Turso]])
- **Architecture**: Actor hierarchy with hard limits (≤15 message variants, ≤20 struct fields) to prevent the Go coordinator's god-object evolution
- **Pitfalls**: Second System Effect is the #1 risk — resist implementing all deferred Go features at once
- **Features**: Table stakes first (worktree isolation, task queue, review gates), differentiating features later (multi-model routing, circuit breakers, attribution loops)

**Recommendation**: Every design decision should pass the "is this the simplest thing that works?" test. The Go server's complexity wasn't planned — it accumulated from reasonable-sounding incremental additions. Hard structural limits (actor boundaries, field counts, explicit scope gates) are the antidote.

### 2. The Compiler Is the Quality System

The decision to rewrite in Rust ([[Language Selection — Compiler as AI Code Reviewer]]) has implications across every dimension:

- **Stack**: Tokio's cooperative scheduling catches blocking-in-async at review time; the borrow checker prevents data races that plagued Go's goroutines
- **Architecture**: Typestate pattern (compile-time state machine) makes illegal task transitions unrepresentable — this is impossible in Go
- **Pitfalls**: AI-generated Go code has 2x more concurrency bugs than human code, and Go's compiler catches none of them; Rust catches most at compile time
- **Features**: Code review agents can focus on logic/architecture rather than catching low-level concurrency bugs the compiler already rejects

**Recommendation**: Lean into Rust's type system aggressively. Use typestate for task lifecycle at the service layer. Use `#[must_use]` and newtype wrappers liberally. The compiler is the first and most reliable code reviewer.

### 3. Agent Topology Is Non-Negotiable

Research across features and pitfalls strongly agrees: flat agent pools fail, hierarchical topology succeeds.

- **Features**: Planner/Worker/Judge hierarchy is table stakes; 57% of companies already run agents in production
- **Pitfalls**: >4 unstructured agents drop compound success rate to ~35%; DORA 2025 shows 9% more bugs and 154% larger PRs without quality controls
- **Architecture**: Three agent types (worker, task reviewer, epic reviewer) with clear responsibilities and the actor pattern to enforce boundaries

**Recommendation**: v1 must ship with all three agent types and strict review gates. Do not ship worker agents without task review — the data is overwhelming that unreviewed AI code degrades quality.

### Actor Scope Clarification

The actor hierarchy has specific multiplicity:
- **CoordinatorActor**: 1× global — dispatch decisions need a global view (model capacity, session counts, cross-project task readiness)
- **AgentSupervisor**: 1× global — session limits are global (e.g., max 8 total, max 3 per model). Holds a HashMap of running sessions.
- **GitActor**: 1× per project/repo — git serialization is per-repo (`.git/index.lock` is repo-scoped). Parallel projects can run git ops concurrently.
- **Event broadcasting**: Handled by repository's `broadcast::Sender` → SSE stream. Not a separate actor.
- **Agent sessions**: Subprocesses managed by AgentSupervisor, each with a lightweight tokio monitoring task for stdout/exit detection. NOT actors.

### 4. The Database Is the Product

Switching to rusqlite + WAL ([[Database Layer — rusqlite over libsql/Turso]]) simplifies the entire system but also means the database layer must be done right because everything depends on it:

- **Single DB** at `~/.djinn/` — tasks, epics, memory, projects, settings, activity, model health
- **WAL mode** for cross-process reads — desktop opens read-only, server pushes MCP events as change signals
- **FTS5** for memory search — bundled in rusqlite, works in WAL
- **Hand-rolled migrations** with `include_str!` — neither sqlx nor refinery support rusqlite natively in async context
- **sqlite-vec** for vector search in v2

**Connection discipline is critical**: All writes through a single connection with `BEGIN IMMEDIATE`. Periodic WAL checkpoint. `busy_timeout=5000` on every connection. This is simple code but must be correct — a single mistake (forgetting `BEGIN IMMEDIATE`, using a connection pool for writes) causes `SQLITE_BUSY` errors under load.

## Tensions and Resolutions

### Tension 1: Feature Richness vs. Scope Control

**Features research** identified 10 table stakes and 8 differentiating features. **Pitfalls research** warns that the Go server accumulated complexity from "just add a column" thinking and that blocking features not in the Go server for 6 months is essential.

**Resolution**: v1 implements a strict subset — core MCP server, task board, memory, agent orchestration (3 types), review system, git integration. Differentiating features (multi-model routing, circuit breakers, attribution loops) are v2. The Go server's 32 migrations document production edge cases — treat them as spec, but implement only what v1 requires.

### Tension 2: Agent Autonomy vs. Quality Control

**Features research** shows the market trending toward "fire and forget" async agents. **Pitfalls research** shows this produces 9% more bugs without review gates.

**Resolution**: Async dispatch with mandatory review gates. Agents work autonomously but task review checks acceptance criteria + code before approval. Epic review checks aggregate quality before an epic can close. This is the Planner/Worker/Judge pattern — autonomy bounded by verification.

### Tension 3: Local Simplicity vs. Deployment Flexibility

**Stack/Architecture research** shows three deployment modes (local, WSL, VPS) with different requirements. **ADR-002** chose rusqlite + WAL which is optimal for local but doesn't replicate for VPS.

**Resolution**: v1 targets local and WSL only. VPS is explicitly v2+. For WSL, the desktop tries direct file access first and falls back to MCP tool reads if the `-shm` shared memory doesn't work across the 9P boundary. This keeps v1 simple while not closing the door on VPS later.

### Tension 4: Async Rust vs. SQLite's Sync Nature

**Stack research** recommends Tokio for the async runtime. **Pitfalls research** and the alternatives research both flag that async SQLite writes are a footgun — SQLx's async model interacts poorly with SQLite's synchronous locking.

**Resolution**: Use `spawn_blocking` for all rusqlite writes within Tokio. This is actually a feature, not a workaround — it prevents holding write locks across `.await` points. Read queries can also use `spawn_blocking` or a blocking thread pool. The actor pattern naturally fits here: the "DB actor" runs writes sequentially on a dedicated thread, receives messages via channel.

## Open Questions

1. **WSL `-shm` compatibility**: Does SQLite's shared memory file work when the desktop on Windows opens the DB via `\\wsl$\`? Needs empirical testing. Low risk — fallback path exists.

2. **rusqlite + FTS5 tokenizers**: Can we load custom FTS5 tokenizers for better search relevance? Need to verify `fts5_tokenizer` registration API in rusqlite.

3. **sqlite-vec maturity**: Is `asg017/sqlite-vec` production-ready enough for v2 vector search? Need to evaluate performance benchmarks and Rust loading ergonomics. Not blocking for v1.

4. **License token format**: Ed25519 JWT with device fingerprinting and short expiry (30 days). Need to decide: custom JWT validation or use a crate like `jsonwebtoken`? Pitfalls research warns against `alg: none` attacks — must assert algorithm from config.

5. **ractor vs hand-rolled actors**: Architecture research recommends `ractor` for agents (restart-on-panic supervision) and hand-rolled for simple actors. Need to evaluate `ractor`'s current state and whether the supervision benefit justifies the dependency.

## Recommendations for Requirements Phase

1. **Group requirements by domain** (task board, memory, agents, git, MCP, licensing) — these map to epics
2. **Every v1 requirement must trace to either the brief or a Go server behavior** — no new features
3. **Mark differentiating features as v2** — multi-model routing, circuit breakers, attribution, vector search, VPS mode
4. **Include "connection discipline" as a non-functional requirement** — it's foundational and easy to get wrong
5. **Include a "Go parity checklist"** requirement — enumerate which Go server behaviors v1 must replicate and which are intentionally dropped (phases, stacked branches, CDC)

## Relations
- [[Project Brief]] — project vision and constraints
- [[Stack Research]] — crate versions and API patterns
- [[Features Research]] — market landscape and feature prioritization
- [[Architecture Research]] — system design patterns
- [[Pitfalls Research]] — risks and anti-patterns
- [[Database Layer — rusqlite over libsql/Turso]] — ADR superseding Turso
- [[Language Selection — Compiler as AI Code Reviewer]] — ADR driving language choice
- [[Embedded Database Survey]] — original DB survey (superseded by ADR-002)