---
title: Cognitive Memory Systems Research
type: research
tags: []
---

# Cognitive Memory Systems Research

Comparative analysis of four approaches to agent memory/context, evaluated for Djinn's multi-agent coordinator architecture (hundreds of concurrent agents, thousands of tasks).

## Systems Analyzed

| System | Core Model | Storage | Retrieval | Multi-Agent | Scale |
|--------|-----------|---------|-----------|-------------|-------|
| **MuninnDB** | Neuroscience (ACT-R, Hebbian) | Custom binary (ERF) + Pebble LSM | 6-phase: FTS + HNSW + temporal + graph + RRF fusion | Single-writer, multi-reader | 100M engrams |
| **Augment Code** | Engineering infra | Cloud: BigTable + custom embeddings + quantized ANN | Hierarchical: broad → focused → dependency | Living specs as shared mutable state | 100M+ LOC |
| **Letta (MemGPT)** | OS memory model | Git repo of markdown + SQLite cache | 4-tier hierarchy: blocks → files → archival → RAG | Git worktrees per subagent | ~100 files/agent |
| **GitHub Copilot** | Citation-verified facts | Repo-scoped DB | Memory load → just-in-time citation check | Review agent writes, coding agent reads | Per-repo |

## MuninnDB — Cognitive Science Approach

### Core Innovation: Storage-Layer Cognition
Five primitives run continuously on every memory unit ("engram"):

1. **ACT-R Temporal Priority**: `B(M) = ln(n+1) - 0.5 × ln(ageDays/(n+1))`. A memory accessed 13× in 10 days has 37× temporal advantage over one accessed once 1400 days ago. Computed at query time from stored access counts — no background mutation.

2. **Hebbian Association Learning**: "Neurons that fire together wire together." Co-retrieved engrams strengthen their association: `w_new = min(1.0, w_old × (1+η)^n)`, η=0.01. Signal is geometric product of both engrams' activation scores — high-confidence co-activations produce stronger associations.

3. **Bayesian Confidence**: Automatically adjusted from evidence: `posterior = (p×s) / (p×s + (1-p)×(1-s))`, with Laplace smoothing to [0.025, 0.975]. Signal strengths: 0.1 (contradiction), 0.65 (co-activation), 0.95 (user confirmation).

4. **Contradiction Detection**: Three modes — structural (O(1) matrix lookup), concept-cluster (FTS overlap + semantic divergence on write), semantic (LLM analysis of candidate pairs).

5. **Predictive Activation Signal (PAS)**: Learns sequential patterns. After activating set A, if B follows, record A→B transitions. Inject predicted candidates into future retrievals.

### 6-Phase Retrieval Pipeline (ACTIVATE)
1. Embed & tokenize query
2. Parallel candidate retrieval: FTS (BM25, field-weighted), HNSW vector, temporal pool, PAS transitions
3. Reciprocal Rank Fusion: `score(d) = Σ 1/(k + rank(d, list_i))`, custom k per signal (FTS=60, HNSW=40, temporal=120)
4. Hebbian boost from co-activation history
5. BFS association traversal with 0.7× hop penalty per hop (2 hops = 49% weight)
6. Final composite scoring: semantic(0.35) + FTS(0.25) + temporal(0.20) + Hebbian(0.10) + access(0.05) + recency(0.05), multiplied by confidence

### Push-Based Triggers
Subscriptions fire on: new_write (above threshold), threshold_crossed (relevance drift), contradiction_detected (highest priority, no rate limiting). Agents subscribe at session start, memories push to context automatically.

## Augment Code — Engineering Approach

### Context Engine Architecture
- Custom-trained code embedding models (not OpenAI/generic)
- Per-user real-time indices, updates within seconds of file changes
- Quantized ANN for 100M+ LOC: 8× memory reduction (2GB → 250MB), 40% latency improvement, 99.9% accuracy parity
- Proof of Possession: SHA hash challenge before returning embeddings (prevents cross-tenant leakage)

### Intent Product — Multi-Agent Orchestration
- **Three-tier hierarchy**: Coordinator → Specialist agents (Investigate, Implement, Verify, Critique, Debug, Review) → Verifier
- **Living specifications**: Mutable state all agents read AND write. Updates propagate to active agents. Not static CLAUDE.md files.
- **Workspace isolation**: Dedicated git branch + worktree per task

### Context Anti-Pattern Research (cited)
- **ETH Zurich (Feb 2026)**: Context files *reduce* task success rates vs. no context, increase inference cost 20%+
- **CodeIF-Bench**: Additional repo context actively degrades instruction following
- **ConInstruct (AAAI 2026)**: Claude 4.5 Sonnet detects conflicting instructions 87.3% of the time but silently picks one
- **Prescription**: Compression beats expansion. Document only what agents cannot see.

### LLM-Summarized Git History (Context Lineage)
Raw diffs too large. LLM (Gemini Flash) summarizes each commit at index time. Summaries co-embedded with code chunks in same vector space. ~100 tokens per commit. Enables "why was this built this way?" queries.

### Four-Layer Prompt Infrastructure
1. System prompts (foundation)
2. Tools (availability as constraint — removing tools forces better alternatives)
3. Skills/Guidelines (precedence: templates > workspace > user > skills)
4. User messages (with prompt enhancer pre-pass)

## Letta (MemGPT) — OS Memory Model

### Four-Tier Memory Hierarchy
1. **Memory Blocks** (Tier 1): Key-value stores pinned to system prompt. Always in context. Editable via `memory()` tool. ~50k chars max. Scoped global (persona, human) or per-project.
2. **MemFS Files** (Tier 2): Git repo of markdown with YAML frontmatter. `system/` dir always loaded; other files navigated on demand. Filetree (names + descriptions) always visible as navigation signal.
3. **Archival Memory** (Tier 3): Agent-generated observations, 300 tokens each, vector-searchable. Closest to traditional RAG.
4. **External RAG/MCP** (Tier 4): Unlimited scale via external tools.

### Progressive Disclosure
File tree structure (names + frontmatter descriptions) is always in the system prompt. Content loads only when agent opens a file. This is the key context management insight — agents know what exists without paying token cost for content.

### Memory Lifecycle Operations
- **Init**: Spawns concurrent subagents in git worktrees to explore codebase, each writes findings, merges back
- **Remember**: Explicit persistence via tool call, classified by type
- **Reflection (Sleep-Time Agent)**: Background process reviews conversation history, extracts learnings as new memory files with git commits
- **Defrag**: Subagent reorganizes — splits large files, merges duplicates, restructures hierarchy. Target: 15-25 focused files.

### Multi-Agent Coordination
Git worktrees per subagent. Concurrent reads/writes without conflicts. Standard git merge on completion. Shared memory blocks across agents.

## GitHub Copilot — Citation-Verified Memory

### Memory Schema
```
Subject: what aspect is being remembered
Fact:    the concrete observation or pattern
Citations: ["src/client/sdk/constants.ts:12", "src/server/routes/api.ts:45"]
Reason:  why this matters for future tasks
```

### Just-In-Time Citation Verification
On memory load, citations checked against live codebase. If code changed and contradicts memory, memory corrected on the spot. No offline curation, no staleness.

### Cross-Agent Memory Flow
Review agent writes memories → coding agent reads and applies → CLI agent consumes during debugging. Single repo-scoped knowledge base.

## Git-as-Context Patterns

### DiffMem (Growth-Kinetics)
Two-layer storage: current Markdown files (BM25 indexed) for "now" view; git commit graph for temporal depth (queried on-demand via targeted `git diff`). Writer agent stages atomic commits for traceability.

### Git Context Controller (arXiv 2508.00031)
Agent context managed via git-inspired operations: COMMIT (checkpoint reasoning), BRANCH (fork reasoning path), MERGE (combine trajectories), CONTEXT (hierarchical retrieval). >80% SWE-Bench Verified.

### Augment Context Lineage
LLM-summarized commits co-embedded with code. Enables pattern reuse ("show me a commit that added a similar feature flag").

## Key Insights for Djinn

### What Djinn Has That Others Don't
**Tight coupling between task board and knowledge base.** Every task references memory notes. Task completion = evidence for/against patterns. Hundreds of task sessions create massive co-access data. The task graph provides topological scope for context retrieval. This feedback loop is unique.

### Critical Gaps
1. **No temporal priority** — `last_accessed` tracked but unused in ranking
2. **No access frequency tracking** — no `access_count` column
3. **No implicit associations** — only manual wikilinks
4. **No confidence scoring** — all notes treated equally regardless of evidence
5. **No context compression** — `build_context` returns everything linked
6. **No progressive disclosure** — full content on every read
7. **No contradiction detection** — agents can write conflicting knowledge
8. **No push-based notifications** — agents poll for context
9. **No vector/semantic search** — FTS only (DB-08 deferred to v2)
10. **No session reflection** — completed sessions don't automatically produce knowledge

## Relations
- [[Project Brief]]
- [[Roadmap]]
- [[V1 Requirements]]
- [["ADR-030: Repo-Committed Verification and Commit-Hash Caching"]]
- [[decisions/adr-008-agent-harness-—-goose-library-over-summon-subprocess-spawning|ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]]
- [[ADR-015: Session Continuity & Resume]]
