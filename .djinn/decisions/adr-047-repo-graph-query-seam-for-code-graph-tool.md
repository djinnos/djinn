---
title: ADR-047: Repo-graph query seam for code_graph tool
type: adr
tags: ["adr-044","code-graph","architecture"]
---


# ADR-047: Repo-graph query seam for `code_graph` tool

## Status
Accepted

## Context

ADR-044 Phase 3 requires a `code_graph` tool in `djinn-agent` that queries the repository dependency graph. Task `4pes` discovered that `djinn-agent` cannot consume the repo-graph functionality because it lives in `server/src/repo_graph.rs` — a private server module. Creating a `djinn-agent → djinn-server` dependency would be circular.

This spike determines the sanctioned crate/interface seam.

## Decision

**Use the existing MCP tool bridge pattern (ADR-041) without extracting a new shared crate.**

### Why no shared crate

- `RepoDependencyGraph` and its types are already `pub` in `server/src/repo_graph.rs`
- The heavy dependencies (petgraph, SCIP protobuf parser) should stay in the server crate
- Extracting types to `djinn-core` would pull `petgraph` into every crate that depends on core
- The existing tool-bridge pattern already handles this exact scenario for memory, tasks, LSP, etc.

### Architecture

```
djinn-agent (extension/mod.rs)
    │ dispatches "code_graph" tool call
    ▼
djinn-mcp (tools/graph_tools.rs)          ← NEW: tool handlers + request/response types
    │ calls trait methods on McpState
    ▼
RepoGraphOps trait (defined in djinn-mcp) ← NEW: query trait
    │ implemented by server
    ▼
server/src/repo_graph.rs                   ← UNCHANGED: RepoDependencyGraph stays here
```

### Minimal API surface

The `RepoGraphOps` trait needs four operations matching the `code_graph` tool's query modes:

```rust
#[async_trait]
pub trait RepoGraphOps: Send + Sync {
    /// Neighbors of a file or symbol node (edges in/out).
    async fn neighbors(&self, key: &str, kind: Option<&str>) -> Result<Vec<GraphNeighbor>>;
    /// Top-ranked nodes by PageRank + structural weight.
    async fn ranked(&self, kind_filter: Option<&str>, limit: usize) -> Result<Vec<RankedNode>>;
    /// Symbols that implement a given symbol.
    async fn implementations(&self, symbol: &str) -> Result<Vec<String>>;
    /// Transitive impact set — what depends on this node.
    async fn impact(&self, key: &str, depth: usize) -> Result<Vec<String>>;
}
```

Response types (`GraphNeighbor`, `RankedNode`) are defined in `djinn-mcp` as serializable structs — they are the tool's output contract, not the internal graph types.

### Impacted files

| File | Change |
|------|--------|
| `server/crates/djinn-mcp/src/providers/mod.rs` | Add `RepoGraphOps` trait |
| `server/crates/djinn-mcp/src/tools/graph_tools.rs` | **New** — tool handlers (~200 lines) |
| `server/crates/djinn-mcp/src/tools/mod.rs` | Register `graph_tools` module |
| `server/src/server/state/mod.rs` | Store `Arc<RepoDependencyGraph>` in AppState |
| `server/src/mcp_bridge.rs` | Implement `RepoGraphOps` for server adapter |
| `server/crates/djinn-agent/src/context.rs` | Add graph ops to `AgentContext::to_mcp_state()` |
| `server/crates/djinn-agent/src/extension/mod.rs` | Add `code_graph` dispatch case |

### What stays out of scope

- No extraction of `RepoDependencyGraph` or SCIP types to a shared crate
- No diff-threshold or incremental graph patching
- No graph mutation API — queries are read-only against the cached graph
- The `repo_map.rs` rendering path is unchanged (it uses `rank()` directly)

## Follow-on guidance for task `4pes`

1. Create `RepoGraphOps` trait in `djinn-mcp/src/providers/` following the `LspOps`/`GitOps` pattern
2. Define response types in `graph_tools.rs` (not internal graph types)
3. Implement the trait in `server/src/` by wrapping `RepoDependencyGraph` methods
4. Wire into `McpState` and `AgentContext::to_mcp_state()`
5. Add `code_graph` tool schema and dispatch in `extension/mod.rs`
6. The graph is already built and cached by the repo-map watcher — reuse it via `AppState`

## Relations

- [[ADR-044: Interactive Code Intelligence — LSP Enhancements, NamePath Addressing, and Repo Graph Queries]]
- [[ADR-041 MCP tool bridge architecture]]
