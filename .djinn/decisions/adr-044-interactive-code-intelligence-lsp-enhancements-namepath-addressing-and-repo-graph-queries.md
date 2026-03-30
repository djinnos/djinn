---
title: ADR-044: Interactive Code Intelligence — LSP Enhancements, NamePath Addressing, and Repo Graph Queries
type: adr
tags: ["adr","architecture","agent","lsp","scip","repo-graph","code-intelligence","symbol-navigation"]
---





# ADR-044: Interactive Code Intelligence — LSP Enhancements, NamePath Addressing, and Repo Graph Queries

## Status: Draft

Date: 2026-03-27

Related: [["ADR-043: Repository Map — SCIP-Powered Structural Context for Agent Sessions"]], [[ADR-041: Unified Tool Service Layer in djinn-mcp]]

## Context

### Two Layers of Code Intelligence, One Gap

ADR-043 established Djinn's two-layer code intelligence architecture:

- **SCIP (batch)**: Whole-repo structural indexing. Produces a static repo map injected into the system prompt at session start. Gives agents instant orientation — top 12 files, 4 symbols each, PageRank-ranked.
- **LSP (interactive)**: Running language servers (rust-analyzer, typescript-language-server, gopls, pyright) spawned per worktree. Exposes `hover`, `definition`, `references`, `symbols` via the `lsp` tool during execution.

The gap is in between: the repo map is too coarse for targeted queries ("what implements this trait?", "what files depend on this module?"), while the LSP tool is too low-level — agents must navigate by file:line:character positions, receive unfiltered symbol dumps, and have no way to query the rich dependency graph that SCIP already built.

### What We Observed (Serena Comparison)

Serena (an open-source coding agent toolkit) wraps LSP into symbol-level tools. Comparing it with Djinn's current approach revealed three specific patterns worth adopting:

1. **Progressive symbol disclosure**: Serena's `get_symbols_overview` returns symbols grouped by kind with configurable depth. Top-level only at `depth=0`, include children at `depth=1`. On large files, this prevents token bombs. Our `lsp symbols` dumps the entire tree as indented text with no filtering.

2. **Name-path addressing**: Serena's `NamePathMatcher` lets agents reference symbols as `Class/method` or `Module/Type` instead of file:line:character. This eliminates a common multi-step dance: call `symbols` → visually scan output → extract line number → call `definition` with that line.

3. **Interactive graph queries**: Serena uses LSP `references` for cross-file navigation, but it's limited to one-hop. Djinn already has a richer structure — the SCIP-powered petgraph with 8 edge types, PageRank scores, and symbol-level granularity. This graph is currently only used to render the static repo map. Exposing it interactively would give agents dependency analysis, impact queries, and trait-implementation lookups without additional LSP round-trips.

## Decision

### 1. Enhanced LSP Symbols with Progressive Disclosure

Extend the `lsp` tool's `symbols` operation with three new optional parameters:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `depth` | integer | unlimited | Maximum nesting depth. `0` = top-level only. `1` = include direct children. |
| `kind` | string (comma-separated) | all | Filter by symbol kind: function, method, class, struct, interface, enum, variable, constant, module, field, property, constructor, type_parameter. |
| `name_filter` | string | none | Substring match on symbol name. Case-insensitive. |

Output changes to grouped-by-kind format with name-paths included. Fallback shortening: full → children-as-counts → kind-counts-only.

### 2. NamePath Symbol Addressing

Add optional `symbol` parameter to `lsp` tool, usable with `hover`, `definition`, `references` as alternative to `line`/`character`.

Resolution: fetch document symbols → suffix-match name path through tree → use matched position. On ambiguity, return candidates with locations. On zero matches, suggest `lsp symbols`.

Kind hints supported for disambiguation: `fn:rank`, `struct:Config`.

### 3. Repo Graph Query Tool (`code_graph`)

New tool exposing the SCIP-powered petgraph with 4 operations:

- **`neighbors`** — Direct connections of a file or symbol, grouped by edge type
- **`ranked`** — Top-N nodes by PageRank with optional kind/path filtering
- **`impact`** — Transitive dependents via BFS on inbound edges (blast radius)
- **`implementations`** — Find trait/interface implementations via SymbolRelationshipImplementation edges

Graph reflects base commit (not in-progress edits). Agents use LSP for in-progress work, graph for architectural context.

## Implementation

### Phase 1: Enhanced LSP Symbols (~200 lines)
- `lsp.rs` — Kind filtering, depth control, grouped formatting
- `extension/mod.rs` — New parameters in tool schema

### Phase 2: NamePath Addressing (~250 lines)
- `lsp.rs` — `resolve_symbol_to_position()` with document symbol cache
- `extension/mod.rs` — Optional `symbol` parameter, resolver dispatch

### Phase 3: Repo Graph Query Tool (~500 lines)
- New `extension/code_graph.rs` — Tool handler with 4 operations
- `repo_graph.rs` — Public query methods (neighbors, implementations, transitive_dependents)
- `extension/mod.rs` — Register tool, add dispatch

Phases 1 and 3 can be parallelized. Phase 2 builds on Phase 1's document symbol cache.

## Consequences

### Positive
- Fewer exploration turns (NamePath eliminates symbols→scan→hover dance)
- Better architectural decisions (impact/implementations queries)
- ~40-60% token reduction on symbol operations
- No new infrastructure — all data sources already exist

### Negative
- Graph staleness in worktrees (reflects base commit, not edits)
- Name-path ambiguity on common names (mitigated by kind hints)
- Tool surface growth (mitigated by per-role introduction)

## Relations

- [["ADR-043: Repository Map — SCIP-Powered Structural Context for Agent Sessions"]]
- [[ADR-041: Unified Tool Service Layer in djinn-mcp]]