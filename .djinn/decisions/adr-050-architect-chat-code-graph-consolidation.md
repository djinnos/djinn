---
title: "ADR-050: Architect/Chat Code-Graph Consolidation, Canonical SCIP Indexing, and Graph Query Extensions"
type: adr
tags: ["adr","architecture","agent","architect","chat","scip","code-graph","repo-map","performance"]
---

# ADR-050: Architect/Chat Code-Graph Consolidation, Canonical SCIP Indexing, and Graph Query Extensions

## Status: Draft

Date: 2026-04-07

Related: [[ADR-043 Repository Map — SCIP-Powered Structural Context]], [[ADR-044 Interactive Code Intelligence]], [[ADR-046 Chat-Driven Planning]], [[ADR-047 Repo-Graph Query Seam]], [[ADR-034 Agent Role Hierarchy]]

## Context

### Empirical signal: `code_graph` is essentially unused

A query against the local agent session database (`~/.djinn/djinn.db`, 87,947 messages across all historical sessions) shows the actual `tool_use` invocation counts:

| Tool         | Invocations |
|--------------|-------------|
| `read`       | 25,424      |
| `shell`      | 18,934      |
| `lsp`        | 170         |
| `code_graph` | **1**       |

Five roles (worker, reviewer, planner, lead, architect) all have `code_graph` exposed in their tool surface. Across the full history of agent execution, the tool has been called once. Workers and reviewers reach for `read`/`shell grep` reflexively even for questions the graph would answer better (e.g. "what implements this trait", "what uses this symbol"). The schema cost of `code_graph` is paid in every system prompt for every role; the value extracted is zero outside one experimental call.

Meanwhile, the underlying SCIP graph is *not* unused — `repo_map.rs` renders it into a token-budgeted skeleton that ships with every agent session prompt (per ADR-043). The graph is load-bearing as a *passive prompt-cache asset*. The interactive query surface is what has failed to land.

### The chat is the interactive Architect

Reading `chat.md` and `chat_tools.rs` side-by-side with `architect.md` and the architect's tool list, the two roles are functionally identical:

- **Same posture**: read-only on code (`shell`, `read`, `lsp`), full read/write on board + memory.
- **Same outputs**: ADRs, epics, tasks for Planner pickup, requirements, roadmap notes.
- **Same prohibition**: no `write`/`edit`/`apply_patch` (chat_tools.rs:5-6 documents this explicitly).
- **Same execution context**: chat runs against the project root, never a worktree (chat_tools.rs:33-34).

The differences are temporal, not capability-shaped: chat is human-driven and interactive; architect is autonomous and patrol-driven. They should share a tool surface by contract, not by coincidence.

Today they have drifted apart in small ways — for example, `epic_create` is exposed to chat (via the standard MCP tool surface) but not to the architect (`architect_tool_names.snap` shows `epic_show`/`epic_update`/`epic_close`/`epic_tasks` but no `epic_create`). The architect can update epics and create tasks but cannot open a new epic; this forces it to route epic creation through a planning task, adding an unnecessary hop.

### Per-worktree SCIP indexing is wasteful for graph consumers

`server/src/watchers/repo_map.rs` keys SCIP refreshes by `(project_id, project_path, worktree_path, commit_sha)` (`RefreshTarget` line 50-67). It already optimises with `WORKTREE_REUSE_DIFF_FILE_THRESHOLD = 20`: when a worktree diverges from merge-base by ≤20 files, it reuses the canonical index plus a small delta. But for larger divergences a full per-worktree indexer pass runs (60–120 s for `rust-analyzer`), and worker sessions can each trigger one for their own branch.

Once `code_graph` is restricted to architect + chat (both of which run against canonical-main code, not worktrees), the entire *graph index* only needs to exist for canonical-main per commit. Per-worktree SCIP runs become unnecessary for the graph; the rendered repo-map *skeleton* that workers consume can be produced from the canonical graph + a per-worktree filesystem overlay (the `changed_files` already tracked in `WorktreeReusePlan`).

### Capability gaps in the current `code_graph` surface

The four existing operations (`graph_tools.rs:115-200`) — `neighbors`, `ranked`, `impact`, `implementations` — cover only a fraction of what the SCIP graph already encodes. Auditing them against the structural-health workflow we want the Architect to run (god-object detection, dead-symbol sweeps, ADR boundary drift, refactor blast-radius reasoning, incremental patrols) reveals concrete gaps:

1. **No symbol search by name.** `RepoDependencyGraph::symbol_node()` is exact-match only on the SCIP symbol string (`scip-rust . . . AgentSession#`). The Architect does not memorize SCIP IDs. Without name-based lookup, every other operation is unreachable except by greping first — defeating the point of the tool.
2. **No cycles / SCC operation.** The most canonical structural smell SCIP can detect (cyclic module dependencies) is invisible to current ops. Petgraph supplies `tarjan_scc` and `kosaraju_scc` for free; we just don't surface them.
3. **No bulk dead-symbol enumeration.** Detecting orphans today would require N round-trips of `neighbors(direction=incoming)`, one per symbol. A single `orphans` op replaces an O(N) loop with one query.
4. **No path between two nodes.** `impact` answers "what is affected" but not "how does A reach B". For refactor reasoning, the dependency chain matters. Petgraph supplies `astar`/`dijkstra`.
5. **No edge enumeration by path glob.** ADR boundary-drift detection wants "all edges from `crates/foo/**` to `crates/bar/**`". Without this, the Architect would have to walk every file pair manually.
6. **No diff between graphs.** Incremental health sweeps need "what structurally changed since the last patrol". Without this, every patrol re-examines the entire graph from scratch.
7. **`ranked` discards degree information.** `repo_graph.rs:96-97` already computes `inbound_edge_weight` and `outbound_edge_weight` for every scored node, but `bridge.rs:169 RankedNode` drops both fields. God-object detection wants degree-sorted output, not PageRank-sorted; the data is already there, just not plumbed. Trivial fix.
8. **No symbol description.** `ScipSymbol` carries `signature`, `documentation`, and `kind` (`scip_parser.rs:62-71`); none of them are reachable through `code_graph`. When the Architect identifies a hot symbol it has to do a separate `lsp hover` round trip to get the signature.
9. **No file rollup on impact/neighbors output.** Hot symbols can have hundreds of dependent sites. Architects (and humans) think in files, not occurrences. A `group_by="file"` parameter collapses the output and matches the mental model.

Items 1–3 are blockers — without them the workflow does not work. Items 4–6 are high-value (workflow works but is awkward). Items 7–9 are polish that costs almost nothing to add.

## Decision

### 1. `code_graph` is exclusive to Architect + Chat

Restrict the `code_graph` tool to two consumers:

- **Architect** — autonomous patrol form (`prompts/architect.md`, dispatched by the coordinator).
- **Chat** — interactive form (`server/crates/djinn-agent/src/chat_tools.rs`, the Djinn chat surface).

Removed from: worker, reviewer, planner, lead. The four affected `*_tools_section_snapshot.snap` files lose the `code_graph` line on regeneration. Workers/reviewers/planners/lead reclaim ~250 tokens of unused tool schema from their system prompts.

Implementation: gate `tool_code_graph()` registration in `server/crates/djinn-agent/src/extension/tool_defs.rs` on the architect role; add `tool_code_graph()` to the `chat_extension_tool_schemas()` vector and `CHAT_EXTENSION_TOOLS` constant in `chat_tools.rs`; wire the dispatch arm to `handlers::call_code_graph` (which already takes a project path the same way `call_lsp` does, so this is mechanical).

### 2. Architect ↔ Chat capability parity is a contract

Codify the role equivalence as an architectural rule, not an accident:

> **Any code-reading or analysis capability granted to Architect must also be present in Chat, and vice versa.** The chat is the human-facing interactive form of the Architect; the patrol Architect is the autonomous form of the chat. Drift between their tool surfaces is a bug.

Concretely, this ADR closes the existing drift:

- **Add `code_graph` to Chat.** (Item 1 above.)
- **Add `epic_create` to Architect.** Currently chat can open epics and architect cannot, forcing the architect to route through planning tasks. Symmetric with `task_create` (which the architect already has). The architect becomes the *source* of strategic intent (ADRs + epics); the Planner remains the *decomposer* of intent into worker tasks.

Future capability changes to either prompt must update both. A test asserts the symmetric subset (read + analysis tools) is identical between architect's allowed-tools list and chat's tool surface.

### 3. SCIP graph indexing: server-managed, lazy, single-flight, canonical-main

The interactive *graph* (the petgraph used by `code_graph`) is built per `(project_id, commit_sha)` against `origin/main` only. Per-worktree SCIP indexer passes are eliminated entirely. The filesystem-watcher-driven indexer trigger is eliminated entirely. The trigger becomes architect dispatch / chat first use.

#### Indexing location: dedicated server-managed worktree

The user's project root is *not* used for SCIP indexing. The user's project root may be on any branch, may be dirty, may be detached, may be behind `origin/main` — none of those states are appropriate for the strategic, repo-wide view the Architect/Chat needs.

Instead, the server maintains a dedicated indexing worktree at `.djinn/index-tree/` (one per registered project). The server is the only entity that touches it. Before any SCIP run, the server executes:

```
git fetch origin main
git -C .djinn/index-tree reset --hard origin/main
```

The result is a deterministic, clean checkout pinned to the latest known `origin/main` HEAD. Indexer output is keyed by the resulting commit SHA. Worker worktrees are unaffected; the user's project root is unaffected.

A dedicated `CARGO_TARGET_DIR` (e.g. `.djinn/index-tree-target/`) is used for indexer-invoked builds, sharing sccache with the rest of the workspace. This makes warm-cache rebuilds nearly free even when the indexer pulls in `cargo check`.

#### Indexing trigger: lazy, on architect dispatch / chat first use

There is no filesystem watcher trigger for SCIP. The current `watchers/repo_map.rs` SCIP path is removed (the watcher itself can survive for skeleton-render purposes; see below).

The new trigger flow, executed by the coordinator immediately before dispatching an Architect patrol (and by the chat handler the first time `code_graph` is called in a session):

1. `git fetch origin main` in `.djinn/index-tree/`.
2. Resolve `origin/main` HEAD commit SHA.
3. Look up `repo_graph_cache[(project_id, commit_sha)]`.
4. **Cache hit**: load cached graph, proceed to dispatch.
5. **Cache miss**: acquire the indexer lock (single-flight), run SCIP indexers in `.djinn/index-tree/` against this commit, build the graph, persist to cache, release the lock, proceed to dispatch.

In steady state (origin/main hasn't advanced since the last patrol), the architect dispatches with zero indexer cost: graph is already cached. When origin/main advances, exactly one SCIP run happens, reused by every subsequent architect and chat session until origin/main moves again.

**Not per-edit. Not per-worker. Not per-worktree. Not in parallel.**

#### Indexer concurrency cap

A server-wide `IndexerLock` (a single `tokio::sync::Mutex<()>`) ensures at most one SCIP indexer subprocess runs at a time. Additional requests queue. Combined with a capped `CARGO_BUILD_JOBS` (default 4, configurable), this prevents the cc-fanout meltdown observed when two parallel `rust-analyzer scip` runs each kicked off `cargo check --workspace` and pulled in `openssl-sys` builds.

The current `INDEXING_COOLDOWN = 30s` debounce becomes unnecessary under this model and is removed; the cache key + single-flight lock subsume it.

#### What the Architect/Chat actually sees

A direct consequence of this design is that **the Architect and Chat do not see the user's working tree**. They see `origin/main` as of the most recent fetch. This is intentional:

- The Architect's role is *strategic*: it reasons about the canonical state of the codebase that the team shares. Local edits do not affect strategic decisions; they are reviewed by workers/reviewers in PR flow, not by the Architect.
- All Architect tool calls (`read`, `shell`, `lsp`, `code_graph`) resolve against `.djinn/index-tree/` rather than the project root when invoked from the architect role.
- Chat behaves the same way by default, with the same canonical view.
- A future opt-in for chat ("look at my working tree, not main") is reasonable but out of scope for this ADR.

This must be made explicit in both `architect.md` and `chat.md` so the role does not get confused between "what's on disk in front of the user" and "what's in canonical main".

#### Worker repo-map skeleton: separate concern

The rendered *repo-map skeleton* (the prompt-cache asset workers consume in their system prompt, per ADR-043) is a different artifact from the graph. Workers still need it, and it still needs to reflect their worktree state (so they see their own in-progress edits).

The skeleton is now produced by:

1. Loading the canonical-main graph for the worktree's merge-base commit (always cached, since the Architect path keeps it warm).
2. Applying a filesystem overlay for the worktree's `changed_files` (already tracked in `WorktreeReusePlan` line 84).
3. Re-rendering the skeleton with the overlay.

This kills the 60–120 s SCIP indexer pass on every worktree refresh — overlay-rendering is milliseconds. Graph cache and skeleton cache become two distinct concerns with different lifetimes (graph: per `(project, commit)`, server-wide; skeleton: per `(project, worktree, commit)`, as today). The filesystem watcher survives, but only to invalidate skeletons on local edits, not to trigger SCIP runs.

The detailed implementation of the overlay-based renderer is left to a follow-up epic; this ADR defines the boundary.

### 4. New `code_graph` operations

The following operations are added to `code_graph` to make the consolidated Architect/Chat workflow viable. All extend the existing `RepoGraphOps` bridge trait (`djinn-mcp/src/bridge.rs:185-217`) and dispatch through `graph_tools.rs::code_graph`. No existing operations change behaviour; this is purely additive.

#### Blockers (without these, the workflow does not work)

**`search`** — name-based symbol lookup.
```
code_graph(operation="search", query="AgentSession", kind_filter?, limit?)
→ [{ key, kind, display_name, score, file? }, ...]
```
Fuzzy / suffix match against `display_name` on every `RepoGraphNode`. Returns canonical SCIP keys callable by other operations. Implementation: build a `BTreeMap<String, Vec<NodeIndex>>` keyed by `display_name` at graph build time; query is O(log N + k).

**`cycles`** — strongly-connected components of size > 1.
```
code_graph(operation="cycles", kind_filter?, min_size?)
→ [{ size, members: [{ key, display_name, kind }, ...] }, ...]
```
Wraps `petgraph::algo::tarjan_scc`, filters out trivial SCCs (single node, no self-edge). `kind_filter="file"` returns module-level cycles; `kind_filter="symbol"` returns symbol-level cycles (mutual recursion, etc).

**`orphans`** — bulk dead-symbol enumeration.
```
code_graph(operation="orphans", kind_filter?, visibility?, limit?)
→ [{ key, kind, display_name, file }, ...]
```
Returns nodes with zero incoming reference edges. `visibility` filter (`public`/`private`/`any`) distinguishes "definitely dead" (private with no refs) from "possibly dead public API" (public with no internal refs — may be consumed externally). Visibility comes from SCIP `SymbolInformation` flags (currently parsed but not exposed in `ScipSymbol`; this ADR requires plumbing the flag through).

#### High-value (workflow works but awkward without these)

**`path`** — shortest dependency path between two nodes.
```
code_graph(operation="path", from, to, max_depth?)
→ { from, to, hops: [{ key, edge_kind }, ...], length }
```
Wraps `petgraph::algo::astar` over edge weights. Returns "why does A depend on B" as a hop chain.

**`edges`** — enumerate edges matching path globs.
```
code_graph(operation="edges", from_glob, to_glob, edge_kind?, limit?)
→ [{ from, to, edge_kind, edge_weight }, ...]
```
Walks all file→file (or symbol→symbol) edges, filters by globs. Enables ADR boundary-drift detection in a single tool call: `edges(from_glob="server/src/**", to_glob="server/crates/djinn-agent/**")` finds illegal upward references.

**`diff`** — what changed in the graph since last build.
```
code_graph(operation="diff", since="previous"|<commit_sha>?)
→ {
    base_commit, head_commit,
    added_nodes: [...], removed_nodes: [...],
    added_edges: [...], removed_edges: [...]
  }
```
The `repo_graph()` cache holds the most recent canonical-main graph; on rebuild it retains the previous version in memory for one cycle. `since="previous"` diffs against the in-memory predecessor. (Persistent cross-commit diff is left as future work; in-memory single-step diff is enough for incremental patrols against "what's changed since last patrol".)

#### Polish (cheap; bundled)

**`ranked` extensions:**
- New `sort_by` parameter: `pagerank` (current default) | `in_degree` | `out_degree` | `total_degree`.
- `RankedNode` gains `inbound_edge_weight: f64` and `outbound_edge_weight: f64` fields. The data is already computed in `repo_graph.rs:96-97` and discarded at the bridge boundary; this is pure plumbing.

**`describe`** — symbol detail without an LSP round trip.
```
code_graph(operation="describe", key)
→ { key, kind, display_name, signature?, documentation?, file?, range? }
```
Sources `signature` and `documentation` from the existing `ScipSymbol` fields.

**`group_by="file"` on `impact` and `neighbors`:**
Collapses symbol-level results into per-file rollups: `{ file, occurrence_count, max_depth, sample_keys: [...] }`. Reduces output token count substantially on hot queries.

### 5. Architect & Chat prompt updates

`prompts/architect.md` gains a new section, **"Codebase Health Sweep via `code_graph`"**, placed before Strategic ADR Gaps. It directs the architect through the structural patrol (hot-spot scan, blast-radius for hot files, trait-impl audit, dead-symbol sweep, cycle detection, ADR drift check) and ties findings to corrective actions (`memory_write` ADR → `epic_create` → optional planning task seed for the Planner).

`prompts/chat.md` gains a parallel **"Codebase Structural Queries"** section that maps natural-language user questions onto `code_graph` operations ("what depends on X" → `impact`, "what implements this trait" → `implementations`, "what are the most central files" → `ranked`, etc.) and explicitly tells the chat to treat structural analysis findings the same way the Architect would: ADR + epic, then handoff to the Planner.

The chat identity line is updated to make the equivalence explicit:

> You are **Djinn**, an AI project architect for software delivery. In agent patrols this same role runs autonomously as the Architect; here, you are the human-facing interactive form. You read, analyze, plan, and direct — you do not write code. Workers and the Planner pick up the work you create.

## Consequences

### Positive

- **Tool surface matches usage.** The four roles that never used `code_graph` lose its schema cost (~250 tokens per system prompt × every session). The two roles that *should* use it gain a complete query surface.
- **Indexing cost drops dramatically.** Per-worktree SCIP runs disappear. Filesystem-watcher-driven SCIP runs disappear. Indexing happens at most once per `origin/main` commit, single-flight, in a server-managed indexing worktree. Worker dispatch becomes faster (no waiting on per-branch indexer warmup) and total indexer CPU drops by ~Nx where N is concurrent worktrees.
- **Eliminates the parallel-indexer meltdown class of incident.** The trigger this ADR was partly motivated by — two simultaneous `rust-analyzer scip` runs against two worktrees, each fanning out into `cargo check --workspace` → `openssl-sys` cc compiles, melting the user's machine — becomes structurally impossible. Single-flight lock + canonical-only location + no watcher trigger removes every contributor to that scenario.
- **Architect becomes a structural-health engine.** With `cycles`, `orphans`, `search`, and `diff`, the Architect can run a real codebase quality sweep on every patrol and auto-generate epics + ADRs for problems no other tool catches (god objects, cyclic deps, dead public APIs, ADR drift).
- **Chat becomes the interactive validation surface for Architect prompt changes.** Identical tool surface means prompt iterations can be tested in chat with a human in the loop before being promoted to autonomous patrol. Closes the current "iterate by re-running patrols and reading logs" feedback loop.
- **Architect/Chat parity is enforceable.** A test asserting the symmetric subset closes drift permanently.
- **`code_graph` becomes self-sufficient.** Today it requires `shell grep` to find symbol IDs first; with `search`, it stands alone.

### Negative / risks

- **Architect/Chat see `origin/main`, not the user's working tree.** Stated above; intentional. The class of question "what does my in-progress branch look like structurally" is not answerable by the Architect/Chat under this design. Mitigation: workers/reviewers see their worktree state through the rendered repo-map skeleton (which still applies the worktree overlay) and through their own `read`/`shell` calls. If a chat user genuinely needs structural analysis of their local branch, a future opt-in flag can switch the chat to a temporary worktree-pinned index — out of scope here.
- **Disk cost: a dedicated indexing worktree per project.** One extra checkout per registered project under `.djinn/index-tree/`, plus a dedicated `CARGO_TARGET_DIR`. With sccache shared across workspaces, the marginal cost is the source checkout (typically tens to low hundreds of MB) plus an incrementally-rebuildable target directory.
- **First-ever architect dispatch on a project pays the cold-cache cost.** A from-scratch SCIP index (rust-analyzer scip + cargo check on a clean workspace) can take minutes. This is paid once per project per server lifetime, then once per `origin/main` advance. Acceptable.
- **Migration path for graph cache storage.** Splitting graph cache (per-commit, server-wide) from skeleton cache (per-worktree-commit) requires schema work in the existing `repo_map_cache` table or a new `repo_graph_cache` store. Out of scope for this ADR but flagged as a follow-up.
- **In-memory `diff` is single-step.** "Diff against arbitrary historical commit" is not supported by this ADR. If incremental analysis spans multiple patrols and the previous-version slot has been replaced twice, the second-oldest graph is gone. Acceptable for the patrol cadence but worth knowing.
- **Visibility flag plumbing requires touching `scip_parser.rs`.** Currently `ScipSymbol` does not carry the public/private flag from `SymbolInformation`. Adding it is straightforward but is a parser-level change.
- **`search` index doubles graph build memory by a small constant.** A `BTreeMap<String, Vec<NodeIndex>>` over display names is small relative to the existing graph but is non-zero.
- **Workers lose the *option* of `code_graph`.** A worker that genuinely needed it (we have one historical example) now cannot reach for it. Mitigation: workers receive structural context through the rendered repo-map skeleton (richer under this ADR because it derives from the canonical graph), through scoped pattern/pitfall notes (per ADR-043), and through Architect-attached comments on tasks. If a recurring need surfaces, the workflow is "Architect runs `code_graph` and seeds the worker task with the result", not "give workers the tool back".

### Neutral

- **`code_graph` invocation count will jump from 1 to many.** The empirical "one call ever" finding will no longer hold because the Architect prompt actively directs use.

## Alternatives considered

**Keep `code_graph` universal and just train workers to use it.** Rejected. Workers reach for `read`/`shell` reflexively; a five-line prompt instruction will not change that. Eight months of session history confirms the behavioural pattern. Concentrating the tool where the workflow already lives (strategic analysis = Architect) matches actual usage.

**Add `code_graph` to Architect/Chat without removing it from other roles.** Rejected. The schema cost is paid in every session prompt regardless of usage; removing it from non-consumers is a token win with zero downside given the historical usage data.

**Persist graph snapshots in SQLite for arbitrary cross-commit diff.** Deferred. In-memory previous-version is enough for the patrol-cadence use case ("diff against last patrol") and avoids a new persistent store. If multi-step temporal analysis becomes a real need, persistence is a small addition.

**Join coverage data into `code_graph` directly (e.g. `ranked` returns coverage per node).** Rejected for this ADR. Couples the graph layer to the verification layer. Cleaner: Architect calls `code_graph(ranked)` and `verification_results` separately, joins in its prompt logic. Keeps `code_graph` focused on structure.

**Treat chat and architect as separate roles with overlapping but independent tool sets.** Rejected. They are functionally identical (same outputs, same prohibitions, same execution context). Treating them as separate is exactly what produced the current `epic_create`-on-chat-but-not-architect drift. Codifying the parity contract prevents recurrence.

## Implementation order

1. **This ADR lands.**
2. **`RepoGraphOps` trait extension.** Add `search`, `cycles`, `orphans`, `path`, `edges`, `diff`, `describe` method signatures to `djinn-mcp/src/bridge.rs`. Implement on the server-side `RepoGraphHandle` in `server/src/repo_graph.rs` (most are direct petgraph wrappers).
3. **`graph_tools.rs` dispatch.** Extend `CodeGraphParams` with new fields (`query`, `from`, `to`, `from_glob`, `to_glob`, `since`, `min_size`, `visibility`, `sort_by`, `group_by`). Add the new operation arms to the `code_graph` dispatcher. Snapshot tests for each new operation.
4. **`scip_parser.rs` visibility flag.** Plumb `SymbolInformation` visibility through `ScipSymbol`. Required by `orphans`.
5. **Tool gating.** Restrict `tool_code_graph()` registration to architect role in `extension/tool_defs.rs`. Regenerate the four `*_tools_section_snapshot.snap` files. Add `tool_code_graph()` to `chat_tools.rs` (schema vector, name constant, dispatch arm).
6. **`epic_create` for Architect.** Add to architect's allowed-tools list. Regenerate `architect_tool_names.snap`.
7. **Architect/Chat parity test.** Assert the read+analysis tool subset is identical between the two surfaces.
8. **Prompt updates.** `prompts/architect.md` gains the "Codebase Health Sweep" section + corrective-action guidance. `prompts/chat.md` gains the parallel "Codebase Structural Queries" section + identity reframing.
9. **Indexer split (separate epic).** Decouple graph index (canonical-main, per-commit) from skeleton render (per-worktree, overlay-based). Larger blast radius; tracked under its own ADR if necessary or under this ADR's follow-up epic.

Steps 2–8 are a single epic ("Architect/Chat code-graph consolidation"). Step 9 is its own epic ("SCIP indexing decoupling").
