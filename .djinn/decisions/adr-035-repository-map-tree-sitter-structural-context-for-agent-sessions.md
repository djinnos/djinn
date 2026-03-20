---
title: ADR-035: Repository Map — Tree-Sitter Structural Context for Agent Sessions
type: adr
tags: ["adr","architecture","agent","context","tree-sitter","repo-map","prompt-cache","memory","performance"]
---

# ADR-035: Repository Map — Tree-Sitter Structural Context for Agent Sessions

## Status: Draft

Date: 2026-03-18

Related: [[ADR-023 Cognitive Memory Architecture]], [[ADR-029 Vertical Workspace Split]], [[ADR-034 Agent Role Hierarchy]]

## Context

### The Blind Navigation Problem

Worker agents currently explore codebases through tool calls: `Read` to view files, `shell` to run `find`/`ls`/`grep`. A typical task begins with 3–8 exploration tool calls before the agent understands the codebase structure enough to make meaningful edits. Each tool call costs a full LLM round-trip (request + response), and the file contents from early reads persist in the conversation history, consuming context window budget that could be spent on actual work.

Observation from real agent sessions: the same file is often read 2–3 times during a single task (initial exploration, pre-edit, post-edit verification). A 200-line file read 3 times consumes ~600 lines of context for information that could be represented once.

### Prior Art

**Aider's Repo Map** is the most proven solution. It uses tree-sitter to extract symbol definitions (functions, structs, traits, classes) from every file, builds a dependency graph, runs PageRank to rank files by relevance to the current task, then renders the top-ranked symbols as a compact structural skeleton (~1K tokens for a medium repo). Key results:
- Agents navigate accurately from turn 1 with zero exploration tool calls
- The map lives in the static prompt prefix, hitting prompt cache on every turn (90% cost reduction on Anthropic)
- Dynamic sizing: map expands when no files are in context (exploration mode), shrinks when specific files are being edited (focused mode)

**Cursor** uses tree-sitter AST-aware chunking for codebase indexing (vector embeddings), but the agent still reads files via tool calls with no structural context upfront.

**Windsurf** assembles context per-turn from 5 layers (rules, memories, open files, semantic retrieval, recent actions) but doesn't expose a persistent structural map.

### Integration with Cognitive Memory (ADR-023)

The repo map is structural metadata about the project — it describes what code exists and how it connects. This overlaps with the cognitive memory system's purpose of providing contextual knowledge to agents. Rather than building a separate subsystem, the repo map should integrate with the existing memory architecture:

- **Storage**: Repo map data can be stored as a memory note (type `repo_map`) with L0/L1/L2 tiered content
- **Retrieval**: File-level affinity scoring can become an RRF signal — when an agent is working on `src/agent/mod.rs`, notes about that module rank higher
- **Freshness**: The map must be incrementally updated as files change, analogous to how note access counts update on read
- **Caching**: Tree-sitter parsed tags should be cached by file mtime in the DB, similar to how note content is cached in FTS5

### Prompt Cache Synergy

A repo map in the system prompt is an ideal prompt cache candidate:
- Large (1K–4K tokens), placing it above Anthropic's minimum thresholds
- Stable between turns (only changes when files are modified on disk)
- Positioned early in the message prefix (system prompt → tools → repo map → conversation)
- Cache hit = 90% cost reduction on Anthropic, 50% on OpenAI/Fireworks

Without a repo map, the system prompt is often too small to cache effectively. With one, every turn after the first gets a substantial cache hit on the entire static prefix.

## Decision

### 1. Tree-Sitter Symbol Extraction

Use `tree-sitter` (Rust crate v0.26+) with per-language grammar crates to extract symbol definitions and references from source files. Grammar crates embed `TAGS_QUERY` constants — no `.scm` files need to be shipped on disk.

**Initial language support:**
- Rust (`tree-sitter-rust`): structs, enums, traits, functions, methods, macros, modules
- TypeScript/TSX (`tree-sitter-typescript`): classes, functions, interfaces, methods, modules
- Python (`tree-sitter-python`): classes, functions, methods
- Go (`tree-sitter-go`): structs, interfaces, functions, methods
- JavaScript (`tree-sitter-javascript`): classes, functions, methods

Additional languages added incrementally. Unsupported file types are represented as filenames only (no symbol extraction).

**Tag types extracted:**
- `definition.function`, `definition.method`, `definition.class`, `definition.interface`, `definition.module`, `definition.macro`
- `reference.call`, `reference.implementation`

### 2. Dependency Graph + PageRank Ranking

Build a directed graph using `petgraph` (which has built-in `page_rank`):
- **Nodes**: source files (relative paths)
- **Edges**: file A → file B when A references a symbol defined in B
- **Edge weights**: based on reference count (sqrt-dampened) and identifier quality

**Weight multipliers** (adapted from Aider, validated at scale):

| Condition | Multiplier | Rationale |
|-----------|-----------|-----------|
| Base | 1.0 | Default |
| Identifier mentioned in task description | 10× | Task-relevant symbols surface first |
| Well-named identifier (≥8 chars, snake/camel case) | 10× | Meaningful names correlate with important symbols |
| Private identifier (`_`-prefixed) | 0.1× | Internal details deprioritized |
| Symbol defined in >5 files (ubiquitous) | 0.1× | Common names add noise |
| Reference from file currently being edited | 50× | Immediate context is highest priority |

**PageRank personalization**: files referenced in the task description, task memory_refs, and currently-edited files get boosted personalization vectors.

### 3. Token-Budgeted Rendering

The map is rendered as a compact tree of file paths + symbol signatures, fitted to a configurable token budget via binary search:

```
src/agent/mod.rs:
│ pub struct AgentSession
│ pub fn start_session(task: &Task) -> Result<SessionHandle>
│ fn handle_tool_call(call: ToolCall) -> ToolResult
src/agent/provider.rs:
│ pub trait LlmProvider: Send + Sync
│ fn build_request(&self, conv: &Conversation) -> Value
│ fn stream_response(&self, body: Value) -> Stream<Result<String>>
src/db/repositories/note/search.rs:
│ pub fn search(project_id: &str, query: &str) -> Vec<NoteSearchResult>
│ fn fts_candidates(query: &str) -> Vec<(String, f64)>
```

**Budget defaults:**
- With files in context (focused work): `1024` tokens
- Without files (exploration): `1024 × 8 = 8192` tokens (auto-expand)
- Hard cap: `context_window - 4096` tokens

**Binary search fitting**: start at `budget / 25` tags, iterate with 15% error tolerance.

### 4. Storage & Caching

**Three-level cache** (adapted from Aider's proven strategy):

1. **Tag cache (DB-backed)**: Per-file symbol tags cached in a `repo_map_tags` table, keyed by `(project_id, file_path, mtime)`. Invalidated when file modification time changes. Survives process restarts.

2. **Graph cache (in-memory)**: The computed PageRank result, cached per `(project_id, task_id, edited_files)` tuple. Invalidated when edited files change or task changes.

3. **Rendered map cache (in-memory)**: The final text output, cached per graph cache key + token budget. Returned directly on cache hit.

**DB schema:**
```sql
CREATE TABLE repo_map_tags (
    id          TEXT PRIMARY KEY,
    project_id  TEXT NOT NULL REFERENCES projects(id),
    file_path   TEXT NOT NULL,
    mtime       INTEGER NOT NULL,    -- file modification time (epoch seconds)
    tags_json   TEXT NOT NULL,        -- JSON array of {name, kind, line, refs[]}
    UNIQUE (project_id, file_path)
);
CREATE INDEX idx_repo_map_tags_project ON repo_map_tags(project_id);
```

### 5. Integration with Cognitive Memory (ADR-023)

The repo map integrates with the memory system at two levels:

**Level A — Repo map as a memory note:**
- On project add or periodic refresh, store a rendered repo map as a note with `note_type = "repo_map"`
- L0 abstract: "Rust workspace: 4 crates, 52K LOC, 180 files" (~20 tokens)
- L1 overview: Top-ranked files with key symbols (~400 tokens)
- L2 full: Complete rendered map with all symbols
- Agents doing broad exploration (PM, Architect roles) discover this note via `memory_search` or `build_context`

**Level B — File affinity as an RRF signal:**
- Add a 6th RRF signal: **file affinity** (k=80)
- When an agent is working on files `[foo.rs, bar.rs]`, notes whose content references those file paths or whose associated symbols overlap get boosted
- Implementation: extract file paths mentioned in note content, compare with active file set
- This connects structural code knowledge (repo map) with semantic project knowledge (memory notes)

**Level C — Co-access learning:**
- When an agent reads a memory note and then edits certain files in the same session, record the `(note, file_paths)` association
- Over time, this builds implicit connections: "ADR-023 is always read before editing `search.rs`"
- Feeds into the existing Hebbian association mechanism (ADR-023 Phase 17b)

### 6. Lifecycle & Refresh

**Initial build**: On project add, parse all source files in parallel using rayon. Expected time: <1s for 1K files with 8 threads.

**Incremental refresh**: Before each agent session starts, diff file mtimes against the tag cache. Only re-parse changed files. Expected time: <50ms for typical task-level changes.

**Git-aware optimization**: Use `git diff --name-only HEAD~1` to identify changed files since last refresh, avoiding a full directory walk.

**No background daemon**: The map is refreshed synchronously at session start. Given the <50ms incremental cost, a background process adds complexity without measurable benefit.

### 7. Prompt Placement

The repo map is injected into the agent's message array as part of the static prefix, positioned for maximum prompt cache efficiency:

```
[System prompt]         ← cache breakpoint #1 (Anthropic)
[Tool definitions]      ← cache breakpoint #2
[Repo map]              ← cache breakpoint #3
[Task context + memory] ← cache breakpoint #4
[Conversation history]  ← dynamic tail (not cached)
```

The repo map is rendered as a user message + assistant acknowledgment pair (Aider's pattern), ensuring it's a valid conversation turn that doesn't confuse the model.

For providers with automatic caching (OpenAI, Fireworks, DeepSeek), no explicit breakpoints needed — the stable prefix naturally gets cached.

### 8. Parallel Parsing with Rayon

File parsing is embarrassingly parallel. Each file gets its own `Parser` instance (tree-sitter `Parser` is `Send + Sync`):

```rust
let file_tags: Vec<(PathBuf, Vec<Tag>)> = files
    .par_iter()
    .filter_map(|path| {
        let lang = language_for_extension(path.extension()?)?;
        let mut parser = Parser::new();
        parser.set_language(&lang.into()).ok()?;
        let source = std::fs::read(path).ok()?;
        let tree = parser.parse(&source, None)?;
        Some((path.clone(), extract_tags(&tree, &source, &lang)))
    })
    .collect();
```

`Query` objects are compiled once per language and shared across threads (read-only after construction). `QueryCursor` is created per-invocation (cheap).

## Consequences

### Positive

- **Eliminates exploration tool calls**: Agents understand codebase structure from turn 1, saving 3–8 round-trips per task
- **Massive prompt cache savings**: 1K–4K tokens of stable content in the prefix = 90% cost reduction on Anthropic for every subsequent turn
- **Reduces redundant file reads**: Agent knows which files are relevant before reading, reducing the read-edit-read-edit cycle
- **Task-aware ranking**: PageRank personalization surfaces files relevant to the specific task, not just globally popular files
- **Memory integration**: Repo map enriches the cognitive memory system with structural code knowledge, enabling file-affinity scoring
- **Incremental cost is near-zero**: After initial build, refreshes take <50ms

### Negative

- **New dependencies**: `tree-sitter` (0.26), per-language grammar crates (5 initially), `petgraph` (0.6) — adds to compile time and binary size
- **Language coverage gaps**: Unsupported languages get filename-only representation (no symbols). This degrades gracefully but means the map is less useful for polyglot repos with exotic languages
- **Token budget tension**: The repo map competes with other context for the limited context window. Large repos may need aggressive pruning to stay within budget
- **Cache invalidation complexity**: Three-level cache with different invalidation strategies adds operational complexity

### Mitigations

- **Language coverage**: Start with 5 languages covering >90% of typical Djinn projects. `ts-pack-core` (170+ languages) is available as a future upgrade path if needed
- **Budget tension**: Dynamic sizing (8× expansion when no files in context, shrink when focused) adapts to the agent's current phase. Budget is configurable per-project
- **Cache complexity**: Tag cache uses simple mtime comparison (proven pattern from Aider). Graph and render caches are in-memory with clear invalidation keys — no distributed cache coordination needed
- **Binary size**: Grammar crates add ~2–5MB per language. Feature flags can make languages opt-in if binary size becomes a concern

## Implementation Phases

### Phase 1: Core extraction + rendering (no memory integration)
- Add `tree-sitter`, grammar crates, `petgraph` dependencies
- Implement tag extraction with `TAGS_QUERY` per language
- Implement dependency graph construction + PageRank
- Implement token-budgeted rendering with binary search
- Add `repo_map_tags` table + mtime-based caching
- Inject rendered map into agent session system prompt
- Tests: extraction accuracy per language, PageRank correctness, budget fitting

### Phase 2: Task-aware personalization
- Extract identifiers from task title/description/design for personalization vector
- Boost files referenced in task `memory_refs`
- Track currently-edited files during session for dynamic re-ranking
- Dynamic map sizing (expand for exploration, shrink for focused edits)

### Phase 3: Memory integration (ADR-023)
- Store rendered map as `repo_map` note type with L0/L1/L2 tiers
- Add file affinity as 6th RRF signal in `build_context` and `search`
- Record `(note, file_path)` co-access associations for Hebbian learning

### Phase 4: Prompt cache optimization
- Add `cache_control` breakpoints for Anthropic (repo map as breakpoint #3)
- Add `x-session-affinity` for Fireworks (separate task, already tracked)
- Verify cache hit rates via usage response fields
- Add cache keepalive for Anthropic if agent sessions have idle gaps (unlikely per current architecture)

## Appendix: Aider Algorithm Reference

### PageRank Parameters
- Damping factor: 0.85
- Iterations: 20 (petgraph default)
- Personalization: 100/N per chat file, same value for mentioned files
- Chat file reference boost: 50×
- Mentioned identifier boost: 10×
- Named identifier (≥8 chars) boost: 10×
- Private (`_`) penalty: 0.1×
- Ubiquitous (>5 definitions) penalty: 0.1×
- Reference count dampening: sqrt(count)
- Self-edge for unreferenced definitions: weight 0.1

### Token Budget Fitting
- Initial guess: budget / 25 tags
- Binary search with 15% error tolerance
- Token counting: exact for <200 chars, sampled estimation for longer
- Line truncation: 100 chars

### Cache Invalidation
- File tags: invalidated on mtime change
- Graph: invalidated on any file change or context change
- Rendered map: invalidated on graph change or budget change
- Aider also uses: `map_processing_time > 1.0s` → enable cache (auto mode)

## Relations

- [[ADR-023 Cognitive Memory Architecture]] — repo map integrates as note type + file affinity RRF signal
- [[ADR-029 Vertical Workspace Split]] — crate structure is a natural unit for repo map sections
- [[ADR-034 Agent Role Hierarchy]] — Architect/PM roles benefit most from broad repo maps; Workers need focused maps