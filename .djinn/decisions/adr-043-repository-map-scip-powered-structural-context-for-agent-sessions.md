---
title: "ADR-043: Repository Map — SCIP-Powered Structural Context for Agent Sessions"
type: adr
tags: ["adr","architecture","agent","context","scip","repo-map","prompt-cache","memory","performance"]
---

# ADR-043: Repository Map — SCIP-Powered Structural Context for Agent Sessions

## Status: Draft

Date: 2026-03-25

Supersedes: ADR-035 (Tree-Sitter approach, withdrawn)

Related: [[ADR-023 Cognitive Memory Architecture]], [[ADR-029 Vertical Workspace Split]], [[ADR-034 Agent Role Hierarchy]]

## Context

### The Blind Navigation Problem

Worker agents explore codebases through tool calls: `Read` to view files, `shell` to run `find`/`ls`/`grep`. A typical task begins with 3-8 exploration tool calls before the agent understands the codebase structure enough to make meaningful edits. Each tool call costs a full LLM round-trip, and file contents from early reads persist in conversation history, consuming context budget.

Observation: the same file is often read 2-3 times during a single task. A 200-line file read 3 times consumes ~600 lines of context for information representable once.

### Prior Art

**Aider's Repo Map** uses tree-sitter to extract symbol definitions and references, builds a dependency graph, runs PageRank to rank files by relevance, then renders top-ranked symbols as a compact skeleton (~1K tokens). Agents navigate from turn 1 with zero exploration tool calls, and the map hits prompt cache on every turn.

However, Aider's approach is **syntactic only** -- cross-file references are inferred via name matching, not compiler resolution:
- A reference to `Config` could match any of 5 structs named `Config`
- No type hierarchy awareness (can't find trait implementations)
- No understanding of re-exports, type aliases, or module visibility
- Heuristic edge quality degrades PageRank accuracy

**Sourcegraph Cody** uses **SCIP** (SCIP Code Intelligence Protocol) -- a Protobuf-based protocol capturing compiler-accurate semantic information. Language-specific indexers (backed by real compilers/type-checkers) produce `.scip` files with precise definitions, references, type hierarchies, and cross-repository symbol resolution.

### Why SCIP Over Tree-Sitter

1. **Same language coverage**: Our targets (Rust, TypeScript, Python, Go, Java) exactly match languages with mature SCIP indexers.
2. **Compiler-accurate graph**: SCIP edges are real -- produced by `rust-analyzer`, `tsc`, `pyright`, the Go type-checker. No false edges.
3. **Richer data**: Type hierarchies (`Relationship.is_implementation`), symbol kinds, hover documentation, enclosing ranges.
4. **Simpler implementation**: Shell out to an indexer, read one protobuf file, get everything. No grammar crates, no `.scm` query files, no Pygments fallback.
5. **Main is always compilable**: Djinn has CI gates ensuring main never breaks. SCIP requires compilable code -- not a problem.
6. **No grammar compile-time cost**: With SCIP, we only need the `scip` crate and `petgraph`.

| | Tree-sitter (ADR-035) | SCIP (this ADR) |
|---|---|---|
| Cross-file refs | Heuristic name matching | Compiler-accurate |
| Type resolution | No | Yes |
| Find implementations | No | Yes |
| Symbol documentation | No | Yes (hover docs) |
| Dependencies in binary | 5 grammar crates (~15MB) | `scip` + `protobuf` (~small) |
| Indexing speed | 50-200ms incremental | 60-120s full, cached per commit |
| Requires compilable code | No | Yes (mitigated by CI gates) |
| Language coverage | 130+ | 5 mature (matches our targets) |

### SCIP Protocol Overview

SCIP is a Protobuf-based, document-centric protocol:

- **Human-readable symbol strings**: `rust-analyzer cargo djinn_core 0.1.0 AgentSession#start().` -- debuggable, self-describing, enables cross-repo linking
- **Document-centric**: each source file is a `Document` with occurrences and symbol definitions
- **Occurrence roles as bitset**: Definition, Reference, Import, ReadAccess, WriteAccess, Test, etc.
- **Relationships**: `is_implementation`, `is_type_definition`, `is_reference` edges between symbols
- **4-8x smaller than LSIF** (gzipped), **3x faster to process**

### Prompt Cache Synergy

A repo map in the system prompt is an ideal prompt cache candidate:
- Large (1K-4K tokens), above Anthropic's minimum thresholds
- Stable between turns (only changes when files are modified)
- Positioned early in the message prefix
- Cache hit = 90% cost reduction on Anthropic, 50% on OpenAI/Fireworks

## Decision

### 1. SCIP Index Generation

Shell out to language-specific indexers to produce `.scip` files:

| Language | Indexer | Command | Backend |
|---|---|---|---|
| Rust | `rust-analyzer` | `rust-analyzer scip .` | Chalk solver |
| TypeScript/JS | `scip-typescript` | `scip-typescript index` | `tsc` |
| Python | `scip-python` | `scip-python index . --project-name X` | Pyright |
| Go | `scip-go` | `scip-go` | Go type-checker |
| Java/Kotlin | `scip-java` | `scip-java index` | SemanticDB |

On project add, detect which indexers are available on PATH. Missing indexers degrade gracefully. For multi-language projects, run each applicable indexer and merge `Index` documents into a single graph.

#### 1a. Monorepo-Aware Workspace Discovery

Indexers must run from the correct workspace root, not the project root. In a monorepo (e.g., `djinn/` with `server/`, `desktop/`, `website/` sub-projects), there is no root `Cargo.toml` or `tsconfig.json` -- running indexers from the project root will fail silently or miss sub-projects entirely.

Before indexing, discover sub-workspace roots per language:

| Language | Discovery | Example |
|---|---|---|
| Rust | Find `Cargo.toml` files containing `[workspace]` | `server/Cargo.toml` |
| TypeScript/JS | Find `tsconfig.json` or `package.json` with `workspaces` | `desktop/package.json`, `website/package.json` |
| Python | Find `pyproject.toml` or `setup.py` | `services/ml/pyproject.toml` |
| Go | Find `go.mod` | `tools/cli/go.mod` |
| Java/Kotlin | Find `build.gradle` or `pom.xml` | `backend/build.gradle` |

For each discovered workspace root, run the applicable indexer with `cwd` set to that root. Merge all resulting SCIP `Document` entries into a single dependency graph, adjusting file paths to be relative to the project root (not the workspace root).

#### 1b. Index on Project Add and Server Startup

Indexing must not wait for the first file change event. Trigger initial indexing:

- **On `project_add`**: immediately after registering the project, spawn background indexing so the first session already has a repo map.
- **On server startup**: for each registered project, check if a cached index exists for the current HEAD. If not, spawn background indexing.

### 2. SCIP Index Parsing

Use the `scip` Rust crate (v0.7.0, Apache 2.0):

```rust
use scip::types::{Index, Occurrence, SymbolRole};
use protobuf::Message;

fn load_scip_index(path: &Path) -> Result<Index> {
    let bytes = std::fs::read(path)?;
    Ok(Index::parse_from_bytes(&bytes)?)
}

fn is_definition(occ: &Occurrence) -> bool {
    (occ.symbol_roles & SymbolRole::Definition as i32) != 0
}
```

Extract:
- **Definitions**: `(file_path, symbol_string, kind, line, documentation)`
- **References**: `(file_path, symbol_string, line)`
- **Relationships**: `(symbol_a, symbol_b, relation_type)` from `SymbolInformation.relationships`

### 3. Dependency Graph + PageRank Ranking

Build a directed graph using `petgraph`:
- **Nodes**: source files (relative paths)
- **Edges**: file A -> file B when A references a symbol defined in B (compiler-accurate)
- **Edge weights**: reference count (sqrt-dampened) x symbol importance

**Weight multipliers** (adapted from Aider, enhanced with SCIP data):

| Condition | Multiplier | Rationale |
|-----------|-----------|-----------|
| Base | 1.0 | Default |
| Symbol mentioned in task description | 10x | Task-relevant symbols surface first |
| Symbol is `pub` / exported | 2x | Public API more important than internals |
| Symbol is a trait/interface definition | 3x | Architectural boundaries are high-value |
| Symbol has `is_implementation` relationship | 2x | Impl connections reveal architecture |
| Private/internal symbol | 0.1x | Internal details deprioritized |
| Symbol defined in >5 files (ubiquitous) | 0.1x | Common names add noise |
| Reference from file currently being edited | 50x | Immediate context is highest priority |

**SCIP-enabled signals** (not possible with tree-sitter):
- **Type hierarchy edges**: struct `Foo` implements trait `Bar` -> weighted edge from Foo's file to Bar's file (3x)
- **Re-export following**: SCIP resolves `pub use` to actual definition site, eliminating false edges to barrel files
- **Test file detection**: `SymbolRole::Test` flag -> deprioritize test files (0.2x) unless task is test-related

### 4. Token-Budgeted Rendering

Compact tree of file paths + symbol signatures, fitted to token budget via binary search:

```
src/agent/mod.rs:
| pub struct AgentSession
|   impl AgentSession
| pub fn start_session(task: &Task) -> Result<SessionHandle>
| fn handle_tool_call(call: ToolCall) -> ToolResult
src/agent/provider.rs:
| pub trait LlmProvider: Send + Sync
|   -> impl by AnthropicProvider, OpenAiProvider, FireworksProvider
| fn build_request(&self, conv: &Conversation) -> Value
| fn stream_response(&self, body: Value) -> Stream<Result<String>>
```

SCIP's `Relationship.is_implementation` enables `-> impl by ...` lines -- impossible with tree-sitter.

**Budget defaults:**
- With files in context (focused): `1024` tokens
- Without files (exploration): `8192` tokens (auto-expand)
- Hard cap: `context_window - 4096` tokens

### 5. Storage & Caching

**Two-level cache** (simplified -- SCIP's commit-hash keying eliminates per-file mtime tracking):

1. **SCIP index cache (disk)**: `.scip` file cached per `(project_id, commit_hash)`. Re-indexed only when HEAD changes. Stored in `{workspace}/.djinn/cache/scip/`. Pruned after 5 commits.
2. **Rendered map cache (in-memory)**: Per `(commit_hash, task_id, edited_files, token_budget)`.

```sql
CREATE TABLE repo_map_cache (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    commit_hash TEXT NOT NULL,
    index_path  TEXT NOT NULL,
    indexed_at  TEXT NOT NULL,
    languages   TEXT NOT NULL,
    UNIQUE (project_id, commit_hash)
);
```

#### 5a. Worktree Index Reuse

Full re-indexing (60-120s) for every worktree is wasteful when a branch differs from main by only a few files. Worktrees should inherit from the base branch's index:

1. **Merge-base lookup**: For a worktree branch, find `git merge-base HEAD main` (or the configured default branch).
2. **Cache hit on merge-base**: If a cached index exists for the merge-base commit, reuse it directly. The PageRank ranking will be 99%+ identical for small diffs.
3. **Threshold-based re-index**: Only run a full re-index if the diff vs merge-base exceeds a threshold (e.g., >20 files changed). For small diffs, the base index is good enough.
4. **Future: incremental patching**: For medium diffs, patch the existing graph -- remove/add only changed files' symbols and re-rank. Not in initial scope.
5. **Fallback**: If no base index exists, do a full index as today.

### 6. Integration with Cognitive Memory (ADR-023)

**Level A -- Repo map as a memory note:**
- L0: "Rust workspace: 4 crates, 52K LOC, 180 files" (~20 tokens)
- L1: Top-ranked files with key symbols (~400 tokens)
- L2: Complete rendered map

**Level B -- File affinity as RRF signal:**
- 6th RRF signal: **file affinity** (k=80)
- SCIP enhancement: affinity extends through type relationships -- working on a file implementing `LlmProvider` boosts notes about the trait definition file

**Level C -- Co-access learning:**
- Record `(note, file_paths)` associations for Hebbian learning

### 7. Lifecycle & Refresh

- **Initial build**: On project add and on server startup (if no cached index for HEAD), run all available indexers. 60-120s for medium Rust project (see §1b).
- **Fallback on failure**: If indexer fails (broken compilation), use last successful index.

#### 7a. Tool-Call-Driven Refresh (replaces filesystem watcher)

The original design used a `notify` filesystem watcher on project directories. This is the wrong abstraction: Djinn controls the edit surface (Write/Edit/apply-patch tool calls flow through the MCP server), so it already knows which file changed and which worktree it's in. A filesystem watcher adds unnecessary overhead and scans all worktrees on every event.

**Replace the filesystem watcher with event-driven refresh:**

1. After a Write, Edit, or apply-patch tool call completes, emit a `file:changed` event with the file path.
2. Resolve which project and worktree the path belongs to.
3. Check if HEAD changed for that specific worktree only (e.g., after a commit).
4. If HEAD changed and no cache entry exists for the new commit, spawn background re-indexing for that worktree only (with merge-base reuse per §5a).
5. If HEAD hasn't changed (uncommitted edit), no re-indexing needed -- the SCIP index is commit-keyed, and uncommitted edits don't affect it.

This eliminates the filesystem watcher entirely. All refreshes are targeted, minimal, and triggered only by actual edits through Djinn.

### 8. Prompt Placement

```
[System prompt]         <- cache breakpoint #1 (Anthropic)
[Tool definitions]      <- cache breakpoint #2
[Repo map]              <- cache breakpoint #3
[Task context + memory] <- cache breakpoint #4
[Conversation history]  <- dynamic tail
```

Rendered as user message + assistant acknowledgment pair (Aider's pattern).

## Consequences

### Positive

- **Eliminates exploration tool calls**: Agents understand codebase from turn 1
- **Compiler-accurate graph**: No false edges -- strictly better PageRank
- **Type hierarchy awareness**: Trait -> implementation relationships visible
- **Richer symbol metadata**: Documentation, visibility, kind per token
- **Simpler implementation**: One protobuf parse, no grammar crates
- **Massive prompt cache savings**: 90% cost reduction on Anthropic
- **Simplified caching**: One index per commit hash
- **Clean licensing**: Entire toolchain is Apache 2.0 or MIT

### Negative

- **Indexing latency**: 60-120s full reindex. Mitigated by commit-hash caching, worktree merge-base reuse (§5a), and background indexing.
- **Requires compilable code**: Mitigated by CI gates on main and fallback to last-good index.
- **External tool dependency**: Indexers must be installed. Mitigated by graceful degradation.
- **Supported languages only**: Djinn targets Rust, TypeScript, Python, Go, Java -- all have mature SCIP indexers. Other languages are out of scope.
- **Monorepo discovery heuristics**: Workspace root detection (§1a) relies on conventions (`[workspace]` in Cargo.toml, `workspaces` in package.json). Non-standard layouts may require manual configuration.

## Implementation Phases

### Phase 1: Core SCIP indexing + rendering ✅
- Add `scip` and `petgraph` dependencies
- Indexer orchestration: detect, shell out, collect `.scip` files
- SCIP parsing: definitions, references, relationships
- Dependency graph + PageRank with SCIP-enhanced weights
- Token-budgeted rendering with binary search
- `repo_map_cache` table + commit-hash keying
- Inject rendered map into agent session system prompt

### Phase 2: Task-aware personalization ✅
- Extract identifiers from task for personalization vector
- Boost files from task `memory_refs`
- Track edited files for dynamic re-ranking
- Dynamic map sizing; render `-> impl by ...` lines

### Phase 3: Memory integration (ADR-023) ✅
- `repo_map` note type with L0/L1/L2 tiers
- File affinity as 6th RRF signal with type-relationship extension
- Co-access Hebbian learning

### Phase 4: Prompt cache optimization
- `cache_control` breakpoints for Anthropic *(not yet implemented)*

### Phase 5: Monorepo, worktree reuse, and tool-call-driven refresh
- Monorepo-aware workspace discovery per language (§1a)
- Index on project add and server startup (§1b)
- Worktree merge-base index reuse (§5a)
- Replace filesystem watcher with tool-call-driven refresh (§7a)

## Appendix: PageRank Parameters

- Damping factor: 0.85, Iterations: 20
- Chat file reference boost: 50x, Mentioned identifier: 10x
- Trait/interface: 3x, Implementation relationship: 2x, Public/exported: 2x
- Private (`_`): 0.1x, Ubiquitous (>5 defs): 0.1x, Test file: 0.2x
- Reference count dampening: sqrt(count)
- Self-edge for unreferenced definitions: 0.1

## Appendix: SCIP Symbol String Format

`<scheme> ' ' <manager> ' ' <package> ' ' <version> ' ' <descriptors>`
Example: `rust-analyzer cargo djinn_core 0.1.0 AgentSession#start().`
Suffixes: `/` namespace, `#` type, `.` term, `()` method, `!` macro. Locals: `local <id>`.

## Appendix: Licensing

| Component | License | Usage |
|---|---|---|
| `scip` Rust crate | Apache 2.0 | Protobuf types, linked |
| SCIP protobuf spec | Apache 2.0 | Protocol definition |
| `rust-analyzer` | MIT or Apache 2.0 | External tool |
| `scip-typescript` | Apache 2.0 | External tool |
| `scip-python` | MIT (Pyright) | External tool |
| `scip-go` | Apache 2.0 | External tool |
| `scip-java` | Apache 2.0 | External tool |

## Relations

- [[ADR-023 Cognitive Memory Architecture]] -- repo map integrates as note type + file affinity RRF signal
- [[ADR-029 Vertical Workspace Split]] -- crate structure is a natural unit for repo map sections
- [[ADR-034 Agent Role Hierarchy]] -- Architect/PM roles benefit most from broad repo maps; Workers need focused maps
