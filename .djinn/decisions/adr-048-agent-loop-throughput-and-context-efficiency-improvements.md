---
title: "ADR-048: Agent loop throughput and context efficiency improvements"
type: adr
tags: ["agent-loop","performance","compaction","streaming","context-window"]
---



# ADR-048: Agent loop throughput and context efficiency improvements

## Status
Proposed

## Context

Profiling agent session behaviour reveals several throughput bottlenecks and context-window inefficiencies in the current reply loop:

1. **Sequential tool dispatch** — when the LLM returns multiple `tool_use` blocks in a single turn, every call is dispatched one-at-a-time. Many tools (memory_search, memory_read, task_list, task_show, code_graph, lsp) are read-only and could safely run concurrently.

2. **Blocking on full response before tool dispatch** — the reply loop waits for the complete streamed response to finish before parsing and dispatching tool calls. Tool execution latency stacks on top of generation latency even though individual `tool_use` blocks complete mid-stream.

3. **No lightweight context reclamation** — when approaching context limits the only recourse is full LLM-powered compaction. Old tool results from early turns consume significant tokens but carry diminishing value; clearing them without an LLM call would often be sufficient.

4. **Full-conversation compaction only** — compaction always summarises the entire history. A partial strategy that preserves a stable message prefix would maintain prompt-cache hits while still reclaiming space.

5. **Rate-limit cascade amplification** — during provider rate-limiting, non-critical background requests (note summaries, consolidation LLM calls) continue firing, amplifying the pressure instead of backing off.

6. **Idle-time memory consolidation** — note consolidation currently runs only post-session. Running it during coordinator idle windows (between dispatches) would keep the knowledge base fresher.

7. **Compaction self-overflow** — if the conversation fed to compaction is itself too long for the model's context, compaction fails with no fallback.


## Decision

Implement the following improvements across three priority tiers.

### Tier 1 — High impact

#### 1A. Concurrent read-only tool execution

Annotate each tool schema with a `concurrent_safe: bool` field (defaulting to `false`). In the reply loop, when the LLM returns N tool_use blocks in one turn:

1. Partition into batches: consecutive `concurrent_safe` tools form a parallel batch; any non-safe tool forms a single-item serial batch.
2. Execute parallel batches via `tokio::JoinSet` (bounded by `MAX_TOOL_CONCURRENCY`, default 8).
3. Execute serial batches sequentially.
4. Collect results in original submission order.

Tools marked safe on day one: `memory_read`, `memory_search`, `memory_list`, `memory_build_context`, `memory_associations`, `task_show`, `task_list`, `task_count`, `task_ready`, `task_blocked_list`, `task_blockers_list`, `task_activity_list`, `task_memory_refs`, `task_timeline`, `epic_show`, `epic_list`, `epic_count`, `epic_tasks`, `agent_show`, `agent_list`, `agent_metrics`, `session_show`, `session_list`, `session_messages`, `provider_catalog`, `provider_models`, `provider_connected`, `board_health`, `model_health`, `code_graph`, `output_view`, `output_grep`, `lsp`, `read`.


#### 1B. Streaming tool dispatch / side queries clarification

Side queries (auxiliary read-only lookups) are **not** introduced as a separate message type or hidden provider-side channel. In the current reply-loop architecture they are modeled as ordinary `tool_use` blocks whose schemas are marked `concurrent_safe=true`.

Behavioral contract:

1. When a read-only lookup tool arrives mid-stream, the reply loop may dispatch it immediately using the same streaming/concurrent execution path as other concurrent-safe tools.
2. The lookup result is buffered until the streamed assistant turn is complete.
3. Buffered lookup results are emitted in the normal ordered `tool_result` user message for the next turn, alongside any serial/non-safe tool results from the same assistant turn.
4. This keeps provider tool-call pairing valid and avoids introducing a second "side channel" protocol surface.

Consequence: ADR-048 side-query scope is satisfied by the existing reply-loop/tool-schema architecture once this behavior is explicitly documented and tested; no extra provider primitive or separate result assembly path is required.

#### 1C. Microcompaction pass

Before triggering full LLM compaction, run a zero-cost microcompaction sweep:

1. Walk the conversation from oldest to newest.
2. For tool_result blocks older than N turns (configurable, default 6), replace content with `[Cleared — tool result from turn {n}]`.
3. Exempt the most recent K turns (default 3) from clearing.
4. If the reclaimed token estimate brings context below the compaction threshold, skip full compaction entirely.

This eliminates the most common compaction trigger (accumulated large tool results) without any LLM call.

### Tier 2 — Medium impact

#### 2A. Prefix-preserving partial compaction

Add a partial compaction mode that summarises only the tail of the conversation (messages after a chosen pivot point) while preserving the stable prefix. This keeps the system prompt + early context intact for prompt-cache hits. The pivot point defaults to the message at ~60% of the context window.

When partial compaction is insufficient (reclaimed space < 20% of window), fall back to full compaction.

#### 2B. Rate-limit cascade prevention

When the provider returns 429 or 529:

1. Set a coordinator-level `rate_limited_until: Instant` flag.
2. While the flag is active, skip all non-critical LLM calls: note summary generation, consolidation synthesis, background extraction.
3. Clear the flag after the backoff period expires or the next successful critical request.
4. Respect `Retry-After` headers when present, falling back to exponential backoff with jitter.

#### 2C. Tool schema ordering stability

Sort tool schemas deterministically (alphabetical by name, built-in tools first, MCP tools second) before serializing into the system prompt. This ensures byte-identical prefixes across turns for maximum prompt-cache hit rate. Currently tool ordering may vary due to HashMap iteration order.

### Tier 3 — Lower impact

#### 3A. Idle-time memory consolidation

When the coordinator has no tasks to dispatch and all slots are idle, trigger a consolidation sweep on notes that have accumulated since the last consolidation. Use a minimum cooldown (default 5 minutes) between sweeps to avoid churn. Cancel the sweep immediately if a new task becomes ready.

#### 3B. Compaction overflow retry

If the compaction request itself hits a context-length error:

1. Drop the oldest 20% of message groups from the compaction input.
2. Retry (up to 3 attempts).
3. If all retries fail, fall back to aggressive microcompaction (clear ALL tool results older than 2 turns) and retry once more.

## Consequences

### Positive
- **Reduced per-turn latency**: concurrent tool dispatch + streaming execution removes the serial bottleneck on multi-tool turns.
- **Fewer compaction events**: microcompaction handles the common case (stale tool results) at zero LLM cost.
- **Better prompt-cache utilisation**: partial compaction + stable tool ordering preserve cacheable prefixes.
- **Improved resilience**: rate-limit cascade prevention and compaction overflow retry reduce failure modes.
- **Fresher knowledge base**: idle-time consolidation keeps memory current without blocking task execution.

### Negative
- Concurrent tool dispatch adds complexity to error handling (partial failures, abort semantics).
- Streaming tool dispatch requires careful handling of tool calls that depend on prior tool results within the same turn (rare but possible — the partitioner handles this via serial batches).
- Microcompaction discards information that could theoretically be useful; the turn-based threshold is a heuristic.

### Risks
- Concurrent tool execution could surface latent race conditions in shared state (mitigated: read-only tools by definition don't mutate).
- Aggressive microcompaction might clear results the model was about to reference (mitigated: recent-turn exemption).


## Implementation order

1. **1C** (microcompaction) — lowest effort, immediate payoff
2. **1A** (concurrent tools) — highest throughput impact
3. **2C** (tool schema ordering) — trivial change, measurable cache improvement
4. **2B** (rate-limit cascade) — small change, prevents failure cascades
5. **1B** (streaming dispatch) — highest complexity, largest latency win
6. **2A** (partial compaction) — requires compaction refactor
7. **3B** (compaction overflow) — safety net
8. **3A** (idle consolidation) — coordinator enhancement
